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

use mj_core::config::{Config, HarnessKind, SessionOrder, TargetTemplate as HelTargetTemplate};
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
    ChangedFilesDialog, ConfigIdEditor, ConfirmDialog, Confirmation, ContainerEditor,
    ImportBundleConfirmation, ImportProgress, NoticeLogDialog, RenameEditor,
    RepositoryOriginDialog, TargetActionsDialog, WebDialog,
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
mod keybinds;
mod modal_surface;
mod notify;
pub use notify::Notification;
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

pub use crate::actions::CommandId;
pub use crate::combined::{render_combined, render_combined_with_theme};
pub use crate::dashboard_conversation::SPLIT_REFUSED_NOTICE;
pub use crate::dialogs::{ImportProfileOption, ImportSessionOption};
pub use crate::go::GoMode;
pub use crate::ingest::{
    MaterializedProjectionCache, PreparedMaterializedSessionDetail,
    PreparedMaterializedSessionSummary,
};
pub use crate::keybinds::KeyRoute;
pub use crate::resume::resume_profile_placeholders;
pub use crate::review_settings::{ReviewSettingsChoices, ReviewSettingsDiscoveryResult};
pub use crate::setup::{DetectScope, RejectedRuntime, SetupDetection};
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

/// The Sessions pane's filter: free text and an optional attention state.
///
/// `editing` means typed characters go into `query`; otherwise the pane's
/// plain keys keep their meaning and the filter merely stays in force.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionsFilter {
    pub query: String,
    pub state: Option<SessionStateFilter>,
    pub editing: bool,
}

/// The attention states the Sessions pane can be narrowed to, with the
/// letters that pick them (the same letters as herdr's navigator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStateFilter {
    /// `b`: waiting for input or failed.
    Blocked,
    /// `w`: a turn or background work in progress.
    Working,
    /// `i`: idle with nothing unread.
    Idle,
    /// `d`: finished with an unread answer.
    Done,
}

impl SessionStateFilter {
    pub(crate) fn from_letter(letter: char) -> Option<Option<Self>> {
        Some(match letter {
            'a' => None,
            'b' => Some(Self::Blocked),
            'w' => Some(Self::Working),
            'i' => Some(Self::Idle),
            'd' => Some(Self::Done),
            _ => return None,
        })
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Done => "done",
        }
    }

    pub(crate) fn admits(self, level: AttentionLevel) -> bool {
        match self {
            Self::Blocked => matches!(
                level,
                AttentionLevel::Waiting | AttentionLevel::Unreachable | AttentionLevel::Failed
            ),
            Self::Working => level == AttentionLevel::Working,
            Self::Idle => matches!(level, AttentionLevel::Idle | AttentionLevel::Inactive),
            Self::Done => level == AttentionLevel::Unread,
        }
    }
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
    PinSession {
        session_id: String,
        pane: tile_layout::PaneId,
    },
    UnpinSession {
        session_id: String,
    },
    SplitConversation {
        pane: tile_layout::PaneId,
        direction: ratatui::layout::Direction,
    },
    OpenSessionInSplit {
        session_id: String,
        direction: ratatui::layout::Direction,
    },
    /// Split the focused conversation pane and leave the new pane empty.
    /// The keyboard split falls back to this when no session is selected.
    SplitPane {
        direction: ratatui::layout::Direction,
    },
    /// Remove one conversation pane, saving what it held.
    ClosePane {
        pane: tile_layout::PaneId,
    },
    /// The conversation panes were focused or resized. The controller saves
    /// the arrangement, and a focus move also re-reads which conversation the
    /// keyboard is now in.
    ConversationPanesChanged {
        focus_moved: bool,
    },
    RestartSession {
        session_id: String,
    },
    /// Read the session checkout's branch and changed files on its target.
    /// The host runs it off the loop and answers with `set_git_status`.
    ProbeGitStatus {
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
    /// Ask the host that owns a path field for its completion candidates.
    DiscoverProjects {
        context: String,
        request: mj_core::project_picker::ProjectDiscoveryRequest,
    },
    CompletePath {
        host: mj_core::path_completion::CompletionHost,
        kind: mj_core::path_completion::CompletionKind,
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
        /// The machine the path belongs to, which is what owns a home
        /// directory to expand `~` against.
        machine: Box<mj_core::config::Machine>,
    },
    /// Resolve the automatic build cache values for a machine so the settings
    /// page can show them. `key` identifies the settings resolved.
    PreviewBuildCache {
        generation: u64,
        key: serde_json::Value,
        machine: Box<mj_core::config::Machine>,
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
    Suspend {
        session_id: String,
        acknowledge_unpublished_work: bool,
    },
    DiscardSinceCheckpoint {
        session_id: String,
        checkpoint: mj_core::state::CheckpointMetadata,
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
    /// Read the stored mount and project-directory history on a worker and
    /// hand it back through `DashboardState::apply_mount_history`.
    LoadMountHistory,
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
    CopyNativeSessionId {
        native_session_id: String,
    },
    /// Copy a Mjolnir session's full ID to the clipboard, from the session
    /// action menu or the palette.
    CopySessionId {
        session_id: String,
    },
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
        scope: DetectScope,
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
    /// Interrupt the running turn of this session's open conversation, as
    /// Esc in its composer does.
    InterruptTurn {
        session_id: String,
    },
    /// Show a session's sub-agents in their own workspace. The controller
    /// saves the composer draft first, as the prompt border's click does.
    OpenSubagents {
        parent_id: String,
    },
    LoadNativeAgentHistory {
        owner: String,
        child: String,
        before: Option<(u64, String)>,
    },
    StopNativeAgent {
        owner: String,
        child: String,
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
    CancelWorkspaceClose {
        workspace_id: String,
    },
    CloseWorkspace {
        generation: u64,
        workspace_id: String,
    },
    RecoverWorkspaceDraft {
        generation: u64,
        draft_id: String,
    },
    /// Switch the visible conversation between rendered Markdown and raw text.
    ToggleTranscriptRendering,
    /// Start or stop dictating into the visible conversation's composer.
    ToggleDictation,
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
    Suspending,
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
            Self::Suspending => "Suspending",
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
            Self::Suspending => Some(SessionTransitionKind::Suspending),
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
    /// The selected session's changed files, branch, and upstream distance.
    ChangedFiles(ChangedFilesDialog),
    /// The last notices the footer showed, newest first.
    NoticeLog(NoticeLogDialog),
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
    /// The resolved `[keys]` bindings, refreshed whenever configuration is
    /// replaced so a `config.toml` edit takes effect without a restart.
    pub(crate) keybinds: mj_core::config::Keybinds,
    /// Whether the prefix key has been pressed and the next key completes a
    /// chord. Cleared by that key, by Esc, and by any mouse press.
    pub(crate) prefix_pending: bool,
    pub(crate) resize_mode: bool,
    pub(crate) state: State,
    pub(crate) quotas: BTreeMap<String, ProfileQuota>,
    pub(crate) quota_refreshing: BTreeSet<String>,
    /// The coding agents a look at this machine found, for the Get started
    /// panel a dashboard without an agent profile shows. `None` until a look
    /// has answered, so the panel claims nothing about the machine before it
    /// knows.
    pub(crate) installed_agents: Option<Vec<HarnessKind>>,
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
    /// The build stamped on the workspace pane, as `v2.11.0`. It is a field
    /// rather than the compiled constant so the documentation capture can pin
    /// a placeholder: those screenshots are committed, and a version read from
    /// the binary would make every one of them wrong the moment the next
    /// release goes out.
    pub(crate) version_label: String,
    pub(crate) target_readiness: BTreeMap<String, wizards::TargetReadiness>,
    pub(crate) target_readiness_generation: u64,
    /// The New wizard opened and the stored mount and project history should
    /// be read again, since sessions created after startup add to it.
    pub(crate) mount_history_refresh_pending: bool,
    /// Selection anchor for the Sessions pane, by id rather than position: the
    /// pane shows different row sets at different explicit sizes, so a
    /// position could silently point at a different session after resizing.
    pub(crate) selected_session_id: Option<String>,
    /// The session a refresh took the selection away from, and the row the
    /// clamp put in its place. Nobody chose that row, so a launch finishing
    /// for the displaced session may still take the selection back (R4-11).
    pub(crate) displaced_selection: Option<(String, Option<String>)>,
    pub(crate) command_session_override: Option<String>,
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
    pub(crate) browse_pane: Option<tile_layout::PaneId>,
    pub(crate) pin_ids: BTreeMap<String, u32>,
    pub(crate) pending_browse: Option<String>,
    pub(crate) navigation_session: Option<String>,
    pub(crate) pane_menu: Option<pane_controls::PaneMenu>,
    /// Whether the focused pane fills the conversation band on its own. The
    /// arrangement underneath is untouched, so unzooming puts every pane back
    /// where it was. It is a view state, not part of the stored layout: a
    /// restart comes back unzoomed.
    pub(crate) conversation_zoomed: bool,
    /// The session an attach is running for, while it is still in flight.
    /// The conversation band draws as empty for as long as this is set to a
    /// session other than the one on screen, so the transcript never belongs
    /// to a different row than the highlight.
    opening_session: Option<String>,
    pub(crate) pane_areas: Option<[Rect; DASHBOARD_PANE_COUNT]>,
    /// Whether the last frame was too narrow for a sidebar, so the Sessions
    /// list was stacked above the conversation in its compact form. Read by
    /// the renderer's size checks; set once per frame.
    pub(crate) narrow_layout: Cell<bool>,
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
    /// The Sessions pane's search and state filter, when one is open.
    pub(crate) sessions_filter: Option<SessionsFilter>,
    /// The commands run lately, newest first, for the palette's Recent group.
    pub(crate) recent_commands: std::collections::VecDeque<CommandId>,
    /// The rows the open resume dialog shows, derived from the records, the
    /// scans, and the dialog's own search. Rebuilt where those change and once
    /// a second for the activity labels; empty when no dialog is open.
    pub(crate) resume_rows: Vec<crate::resume::ResumeRow>,
    /// How many rows the current query matched on each tab, indexed by
    /// `ResumeTab::index`. All zero when nothing is typed, and always zero for
    /// the Live tab, which matches names rather than the index. Rebuilt beside
    /// `resume_rows`, from the same merge, so the tabs a person is not looking
    /// at can still say where their hits are.
    pub(crate) resume_hit_counts: [usize; crate::resume::ResumeTab::COUNT],
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
    /// The sessions currently needing a person and whether each has been
    /// reported, for [`DashboardState::notification_events`].
    pub(crate) attention_episodes: BTreeMap<String, crate::notify::AttentionEpisode>,
    pub(crate) viewed_failures: BTreeMap<String, crate::notify::ViewedFailure>,
    pub(crate) drawn_failures: BTreeMap<String, crate::notify::ViewedFailure>,
    /// The pane, row index, and time of the most recent left click on a
    /// session row, so the next click can be recognized as a double click.
    last_row_click: Option<(Focus, usize, Instant)>,
    pub(crate) mode: Mode,
    pub(crate) help_request_generation: u64,
    pub(crate) go: Option<go::GoMode>,
    /// The Git repository `mj` was started in, found before the dashboard
    /// opened. The new-session wizard offers it as the local project.
    pub(crate) launch_project_directory: Option<std::path::PathBuf>,
    pub(crate) go_workspaces: BTreeMap<String, go::GoMode>,
    pub(crate) go_contexts: BTreeMap<String, Result<(std::path::PathBuf, String), String>>,
    /// What each session's checkout looked like when last read, for the
    /// branch on its row and the changed-files overlay.
    pub(crate) git_status: BTreeMap<String, Result<mj_core::local_git::SessionGitStatus, String>>,
    /// When each session's checkout was last asked about, so the host reads
    /// a visible session's status about once a minute and no more.
    pub(crate) git_probe_at: BTreeMap<String, Instant>,
    /// The unreachable-worker notice last shown for each session, so the
    /// notice can be withdrawn once the worker answers again.
    pub(crate) unreachable_notices: BTreeMap<String, String>,
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
    /// Archived transcripts, by wiki id, whose restore has been sent and has
    /// not reported back yet. The restore wizard closes as soon as it sends
    /// the restore, so this, not the wizard, is what keeps a second press or
    /// a reopened wizard from restoring the same transcript twice.
    pub(crate) archive_restores_in_flight: BTreeSet<String>,
    /// The `session_preflight_generation` a live resume's check was sent
    /// under. The check is in flight while that generation is current: every
    /// result of the check, and closing the wizard, moves the generation on.
    /// A restore's hold is the set above instead, because the restore wizard
    /// closes, and moves the generation on, as soon as it sends the restore.
    pub(crate) resume_preflight_generation: Option<u64>,
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
    pub(crate) native_agents: BTreeMap<String, native_agents::NativeAgentPane>,
    /// Stored conversations of Mjolnir sub-agents that have stopped, drawn
    /// read-only because there is no worker to attach to.
    pub(crate) stopped_subagents: BTreeMap<String, stopped_subagents::StoppedSubagentPane>,
    /// Sub-agents whose records left because their parent's suspend stopped
    /// them, for the host to let their conversations go.
    stopped_by_suspend: StoppedBySuspend,
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
    closing_workspaces: BTreeSet<String>,
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
mod pane_controls;
pub use dashboard_sessions::{AttentionEntry, AttentionLevel};
mod dashboard_standby;
mod dashboard_workspaces;
pub use dashboard_workspaces::StoppedBySuspend;
mod native_agents;
mod stopped_subagents;

impl DashboardState {
    pub fn finish_spinner_style_save(&mut self) {
        self.spinner_save_pending = false;
    }

    pub fn new(config: Config, state: State, quotas: BTreeMap<String, ProfileQuota>) -> Self {
        let mut dashboard = Self {
            keybinds: config.keybinds(),
            prefix_pending: false,
            resize_mode: false,
            config,
            state,
            quotas,
            quota_refreshing: BTreeSet::new(),
            installed_agents: None,
            session_details: BTreeMap::new(),
            unreachable_sessions: BTreeSet::new(),
            session_reviews: BTreeMap::new(),
            sessions_with_review: BTreeSet::new(),
            project_sources: BTreeMap::new(),
            session_order_cache: RefCell::default(),
            checkpoint_archive_sizes: BTreeMap::new(),
            go: None,
            launch_project_directory: None,
            go_workspaces: BTreeMap::new(),
            go_contexts: BTreeMap::new(),
            git_status: BTreeMap::new(),
            git_probe_at: BTreeMap::new(),
            unreachable_notices: BTreeMap::new(),
            session_operations: BTreeMap::new(),
            standby_prompts: BTreeMap::new(),
            launch_standby: None,
            launch_standby_anchor: None,
            move_operations: BTreeMap::new(),
            capacity_details: BTreeMap::new(),
            version_label: concat!("v", env!("CARGO_PKG_VERSION")).to_owned(),
            target_readiness: BTreeMap::new(),
            target_readiness_generation: 0,
            mount_history_refresh_pending: false,
            selected_session_id: None,
            displaced_selection: None,
            command_session_override: None,
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
            browse_pane: None,
            pin_ids: BTreeMap::new(),
            pending_browse: None,
            navigation_session: None,
            pane_menu: None,
            conversation_zoomed: false,
            opening_session: None,
            pane_areas: None,
            narrow_layout: Cell::new(false),
            conversation_pane_areas: Vec::new(),
            conversation_area: None,
            resume_sessions_area: None,
            frame_surfaces: FrameSurfaces::new(),
            surface_form: RefCell::new(mj_chat::components::Form::default()),
            session_action_focus: None,
            session_menu_ids: Vec::new(),
            sessions_filter: None,
            recent_commands: std::collections::VecDeque::new(),
            resume_rows: Vec::new(),
            resume_hit_counts: [0; crate::resume::ResumeTab::COUNT],
            session_row_areas: Vec::new(),
            project_heading_areas: Vec::new(),
            pane_size_control_areas: Vec::new(),
            pane_maximize_enabled: [true; DASHBOARD_PANE_COUNT],
            collapsed_project_keys: BTreeSet::new(),
            attention_episodes: BTreeMap::new(),
            viewed_failures: BTreeMap::new(),
            drawn_failures: BTreeMap::new(),
            last_row_click: None,
            mode: Mode::Dashboard,
            help_request_generation: 0,
            modal_click_transition: None,
            suppress_modal_release: false,
            review_settings_generation: 0,
            spinner_save_pending: false,
            review_settings_choices: BTreeMap::new(),
            session_preflight_generation: 0,
            next_move_preparation_request_id: 0,
            archive_restores_in_flight: BTreeSet::new(),
            resume_preflight_generation: None,
            notices: Notices::default(),
            workspace_name: String::new(),
            workspace_names: BTreeMap::new(),
            workspace_order: vec![mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned()],
            active_workspace_id: Some(mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned()),
            subagent_parent_id: None,
            native_agents: BTreeMap::new(),
            stopped_subagents: BTreeMap::new(),
            stopped_by_suspend: StoppedBySuspend::default(),
            workspace_views: BTreeMap::new(),
            workspace_pane_sizes_modified: BTreeSet::new(),
            workspace_layouts_modified: BTreeSet::new(),
            workspace_tab_areas: Vec::new(),
            subagent_workspace_close_area: None,
            workspace_pane_area: None,
            workspace_hamburger_area: None,
            workspace_control_focus: WorkspaceControlFocus::Tabs,
            workspace_management_generation: 0,
            closing_workspaces: BTreeSet::new(),
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
