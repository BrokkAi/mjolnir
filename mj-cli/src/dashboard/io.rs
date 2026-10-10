//! One-shot background jobs the dashboard starts and the updates they report.
//!
//! Each job runs on a blocking task and answers over the dashboard's single
//! [`DashboardIoUpdate`] channel, so no filesystem, database, or process work
//! ever happens on the render loop. Failures travel as `Err` payloads rather
//! than being dropped.

mod spawn;
pub(crate) use spawn::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mj_controller::database::DetachedSessionDraft;
use mj_core::config::{Config, ProjectBundle};
use mj_core::state::{
    MaterializedSession, MovePreparation, ProjectSourceIdentity, SessionRecord, SessionState, State,
};

use mj_controller::controller::Controller;
use mj_controller::controller::ResumeRepositorySourcePreflight;
use mj_controller::session_manager::SessionManagerControl;
use mj_controller::targets::CancellableProcessExecutor;
use mj_tui::{
    DashboardAction, DetectScope, PreparedMaterializedSessionDetail,
    PreparedMaterializedSessionSummary, RejectedRuntime, RemoteRepositoryPreview,
    ReviewSettingsChoices, ReviewSettingsDiscoveryResult, SessionOperationKind, SetupDetection,
    WebViewerAccess,
};
use mj_tui::{WorkspaceDraftEntry, WorkspaceManagementEntry};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{JoinError, JoinHandle};

use crate::daemon;
use crate::dashboard::{CriticalOperationTracker, DashboardContext};
use crate::import::{DashboardImportSuccess, PendingDashboardImport, persist_imported_session};
use crate::pollers::{
    DashboardLifecycleUpdate, LifecycleSuccess, WorkerRecordPersistence,
    WorkerRecordPersistenceOutcome,
};
use crate::short_id;

/// Everything the dashboard learns from a background job.
pub(crate) enum DashboardIoUpdate {
    RuntimeConfirmationMissing(String),
    ProfileHydration {
        key: String,
        result: std::result::Result<(), String>,
    },
    HelpSearchFinished {
        generation: u64,
        result: std::result::Result<mj_core::help_search::HelpSearchResponse, String>,
    },
    RepositoryRemotesRepaired {
        generation: Option<u64>,
        retry: Box<DashboardAction>,
        result: std::result::Result<(), String>,
    },
    /// A workspace manager snapshot loaded away from the event loop. The
    /// selection hint is set only by a successful create; the dashboard must
    /// still verify the modal generation before applying it.
    WorkspaceClosed {
        generation: u64,
        workspace_id: String,
        /// The daemon's confirmation. The dashboard removes the workspace from
        /// the manager it already shows instead of querying everything again.
        result: std::result::Result<(), String>,
    },
    WorkspaceCloseCancelled {
        result: std::result::Result<(), String>,
    },
    WorkspaceManagement {
        generation: u64,
        result: std::result::Result<WorkspaceManagementResult, String>,
    },
    WorkspacePaneSizes {
        result: std::result::Result<BTreeMap<String, mj_core::workspace::PaneSizes>, String>,
    },
    WorkspaceLayouts {
        result:
            std::result::Result<BTreeMap<String, mj_core::workspace::ConversationLayout>, String>,
    },
    WorkerRecordPersistence {
        operation: WorkerRecordPersistence,
        result: std::result::Result<WorkerRecordPersistenceOutcome, String>,
    },
    MaterializedSessionProjection {
        session_id: String,
        result: std::result::Result<Box<PreparedMaterializedSessionDetail>, String>,
    },
    /// A bounded tail of a resumed session's stored transcript, loaded off the
    /// event loop so the conversation is not blank while the poller catches up.
    TranscriptTailSeed {
        materialized: Box<MaterializedSession>,
    },
    StoredSessionSummary {
        session_id: String,
        result: std::result::Result<PreparedMaterializedSessionSummary, String>,
    },
    /// The stored tail of a stopped sub-agent's conversation, which is read
    /// rather than attached to.
    StoppedSubagentTranscript {
        session_id: String,
        result: std::result::Result<Option<MaterializedSession>, String>,
    },
    ProjectSource {
        session_id: String,
        result: std::result::Result<ProjectSourceIdentity, String>,
    },
    ChatOpened {
        generation: u64,
        assignment: mj_tui::PaneAssignment,
        /// The pane that asked for this conversation. A result whose pane has
        /// since moved on to another session is dropped.
        pane: mj_tui::tile_layout::PaneId,
        session_id: String,
        result: Box<std::result::Result<mj_chat::chat::PreparedChat, String>>,
    },
    /// The daemon refused a review action. The message is a sentence for the
    /// person who pressed the key, so it goes back to the chat that sent it.
    NativeAgentHistory {
        owner: String,
        child: String,
        result: std::result::Result<mj_core::native_agent::NativeAgentHistoryPage, String>,
    },
    NativeAgentStopped {
        owner: String,
        child: String,
        result: std::result::Result<(), String>,
    },
    /// Interrupt all finished sending. Each failure names the session or
    /// native sub-agent view it was for.
    InterruptAllFinished {
        targets: mj_tui::InterruptAllTargets,
        failures: Vec<(String, String)>,
    },
    ReviewRefused {
        session_id: String,
        message: String,
    },
    CreateSession(Box<DashboardCreateSessionUpdate>),
    /// An explicit daemon restart finished. The sentence is what the surface
    /// shows: which build came up, or why no daemon of this build did.
    DaemonRestarted(std::result::Result<String, String>),
    GoSelectionSaved(std::result::Result<(), String>),
    GoContext {
        session_id: String,
        result: std::result::Result<(std::path::PathBuf, String), String>,
    },
    GitStatus {
        session_id: String,
        result: std::result::Result<mj_core::local_git::SessionGitStatus, String>,
    },
    GoPrepared {
        workspace_id: String,
        retry: Box<DashboardAction>,
        result: Box<std::result::Result<(Config, mj_core::go::GoRecipe), String>>,
    },
    RemotePreflight {
        generation: u64,
        launch: Box<DashboardAction>,
        result: std::result::Result<RemotePreflightOutcome, String>,
    },
    RenameSession {
        title: String,
        result: std::result::Result<String, String>,
    },
    ChangeWorkspace {
        session_id: String,
        workspace_name: String,
        result: std::result::Result<(), String>,
    },
    /// A prompt typed into a standby composer and handed to the daemon for
    /// delivery once the session is live.
    StartupPromptQueued {
        session_id: String,
        text: String,
        result: std::result::Result<(), String>,
    },
    /// The daemon's answer to taking a recalled prompt back: `Ok(true)` it is
    /// withdrawn, `Ok(false)` it had already been sent.
    StartupPromptWithdrawn {
        session_id: String,
        text: String,
        result: std::result::Result<bool, String>,
    },
    ContainerSettings {
        session_id: String,
        result: std::result::Result<DashboardMetadata, String>,
    },
    TargetReadiness {
        generation: u64,
        target_id: String,
        result: std::result::Result<(), String>,
        /// The configuration checked, when the check found its local
        /// engine's command not installed.
        absent_engine: Option<mj_core::config::TargetTemplate>,
    },
    /// Whether a configured project's local directory exists; `None` when
    /// the check could not tell.
    ProjectDirectory {
        path: std::path::PathBuf,
        exists: Option<bool>,
    },
    ProjectCatalog {
        context: Option<String>,
        result: std::result::Result<mj_core::project_catalog::ProjectCatalogView, String>,
    },
    MountHistory {
        context: Option<String>,
        result: std::result::Result<
            std::collections::BTreeMap<String, Vec<std::path::PathBuf>>,
            String,
        >,
    },
    TargetTest {
        target_id: String,
        result: std::result::Result<(), String>,
    },
    ConfigRename {
        what: String,
        result: std::result::Result<DashboardMetadata, String>,
    },
    /// The daemon's answer to a Refresh: it accepted the request, or it could
    /// not be reached.
    QuotaRefreshRequested(std::result::Result<(), String>),
    WebAccess {
        generation: u64,
        access: WebViewerAccess,
    },
    /// The resume dialog's stopped sessions. The discovery id is the
    /// dialog's; an answer for one that has closed is dropped there.
    ResumeCandidates {
        discovery_id: u64,
        result: Box<std::result::Result<mj_client::daemon::ResumeCandidates, String>>,
    },
    /// The record of the row picked in the resume dialog.
    ResumeRecord {
        session_id: String,
        result: Box<std::result::Result<Option<SessionRecord>, String>>,
    },
    /// One SessionWiki search result. The request id is the dialog's; an
    /// answer for an older one is dropped there.
    WikiRows {
        request_id: u64,
        result: std::result::Result<mj_client::daemon::WikiSearchPage, String>,
    },
    ResumeTextMatches {
        request_id: u64,
        result: std::result::Result<Vec<mj_client::daemon::SessionTextMatch>, String>,
    },
    /// The daemon's answer to a Sessions filter search of conversation text.
    /// The request id is the dashboard's; an answer for an older one is
    /// dropped there.
    SessionTextMatches {
        request_id: u64,
        result: std::result::Result<Vec<mj_client::daemon::SessionTextMatch>, String>,
    },
    /// One archived session's briefing, for the resume dialog's preview.
    WikiBrief {
        wiki_id: String,
        result: std::result::Result<String, String>,
    },
    /// One archived session's matching passages, for the resume dialog's
    /// preview while a query is active.
    WikiHits {
        wiki_id: String,
        query: String,
        result: std::result::Result<Option<mj_client::daemon::WikiHitTranscript>, String>,
    },
    WebListeners {
        generation: u64,
        result: Result<Vec<mj_tui::WebListenerProcess>, String>,
    },
    WebAccessError {
        generation: u64,
        error: String,
    },
    FirstRunStarted,
    FirstRunConfigured(mj_controller::setup::SetupReport),
    FirstRunChecked(std::result::Result<Option<Vec<String>>, String>),
    SetupSaved {
        generation: u64,
        result: std::result::Result<Config, String>,
    },
    SetupDiscovered {
        generation: u64,
        result: std::result::Result<mj_tui::SetupDetection, String>,
    },
    BuildCachePreviewed {
        generation: u64,
        key: serde_json::Value,
        result: std::result::Result<Option<mj_core::state::BuildCachePreview>, String>,
        install_mbx_available: bool,
    },
    MbxInstalled {
        generation: u64,
        key: serde_json::Value,
        machine_id: String,
        result: std::result::Result<String, String>,
        preview: std::result::Result<Option<mj_core::state::BuildCachePreview>, String>,
        install_mbx_available: bool,
    },
    ArchiveSpacePreviewed {
        generation: u64,
        older_than_days: Option<u32>,
        result: std::result::Result<mj_core::state::ArchiveSpacePreview, String>,
    },
    ReviewSettingsDiscovered {
        generation: u64,
        profile_id: String,
        model: Option<String>,
        result: std::result::Result<ReviewSettingsDiscoveryResult, String>,
    },
    ReviewSettingsChoices {
        generation: u64,
        profile_id: String,
        model: Option<String>,
        choices: mj_controller::review_settings::ReviewCapabilityChoices,
    },
    SpinnerStyleSaved {
        result: std::result::Result<Config, String>,
    },
    DetachedSessionState {
        session_id: String,
        result: std::result::Result<(), String>,
    },
    ReadReceipt {
        session_id: String,
        result: std::result::Result<u64, String>,
    },
    CreatedBundle {
        result: Box<std::result::Result<CreatedBundleUpdate, String>>,
    },
    RemovedBundle {
        bundle_id: String,
        result: std::result::Result<(), String>,
    },
    ImportedSessionApplied {
        result: Box<std::result::Result<ImportedDashboardSessionApply, String>>,
    },
    LifecycleReloaded(Box<LifecycleReloaded>),
    LifecycleCancellation {
        session_id: String,
        result: std::result::Result<(), String>,
    },
    MovePrepared {
        session_id: String,
        request_id: u64,
        result: std::result::Result<MovePreparation, String>,
    },
    CheckpointArchiveSizes {
        generation: u64,
        targets: BTreeMap<String, std::path::PathBuf>,
        sizes: BTreeMap<String, Option<u64>>,
    },
    WorkerDiagnosis {
        session_id: String,
        episode_id: u64,
        result: std::result::Result<Option<String>, String>,
    },
    ProjectDiscovery {
        context: String,
        result: std::result::Result<mj_controller::project_picker::ProjectDiscovery, String>,
    },
    PathCompletions {
        context: String,
        prefix: String,
        result: std::result::Result<mj_core::path_completion::PathCompletion, String>,
    },
    MountValidation {
        context: String,
        source: String,
        result: std::result::Result<(std::path::PathBuf, Option<String>), String>,
    },
    SessionMountValidation {
        generation: u64,
        launch: Box<DashboardAction>,
        result: std::result::Result<Option<(String, String)>, String>,
    },
    ResumeRepositoryPreflight {
        generation: u64,
        launch: Box<DashboardAction>,
        submitted_repository_id: Option<String>,
        result: Box<std::result::Result<ResumeRepositoryPreflightApply, String>>,
    },
    ContainerPathResolved {
        context: String,
        result: std::result::Result<std::path::PathBuf, String>,
    },
    SetupPathResolved {
        generation: u64,
        draft: serde_json::Value,
        path: Vec<String>,
        value: String,
        result: std::result::Result<std::path::PathBuf, String>,
    },
    ProjectValidation {
        context: String,
        directory: String,
        result: std::result::Result<
            (std::path::PathBuf, mj_core::state::ManagedWorktreeOptions),
            String,
        >,
    },
    /// Clipboard providers may use a blocking desktop IPC call. The result
    /// is delivered here after that work finishes on a blocking task.
    ClipboardText(std::result::Result<String, String>),
    /// A copied selection reached the desktop clipboard, or did not. Only a
    /// failure needs reporting: the notice for a successful copy is already
    /// on screen.
    ClipboardWritten(std::result::Result<(), String>),
}

pub(crate) struct ActiveLifecycleOperation {
    pub(super) retirement: Option<super::attachment::ChatRetirement>,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) kind: SessionOperationKind,
    pub(crate) retry_launch: Option<DashboardAction>,
    /// How notices named the session when the operation began. The
    /// completion notice falls back to it when the record is already gone.
    pub(crate) notice_name: String,
}

pub(crate) struct WorkspaceManagementResult {
    pub(crate) entries: Vec<WorkspaceManagementEntry>,
    pub(crate) select_workspace: Option<String>,
    pub(crate) deleted_workspace_id: Option<String>,
}

pub(crate) struct RegisteredDashboardSession {
    retry_launch: DashboardAction,
    session: SessionRecord,
    remembered_container_size: Option<(String, mj_core::state::HostContainerSize)>,
    cancelled: Arc<AtomicBool>,
}

pub(crate) enum DashboardCreateSessionUpdate {
    RemoteRepair {
        bundle_id: String,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
        retry: Box<DashboardAction>,
    },
    Registered(Box<RegisteredDashboardSession>),
    Failed {
        error: String,
        retry_launch: Box<DashboardAction>,
    },
}

pub(crate) enum RemotePreflightOutcome {
    Ready(Vec<RemoteRepositoryPreview>),
    Repair(Vec<mj_core::local_git::LocalRemoteRepair>),
}

pub(crate) struct ImportedDashboardSessionApply {
    harness: &'static str,
    native_session_id: String,
    session: SessionRecord,
    bundle: ProjectBundle,
}

pub(crate) struct CreatedBundleUpdate {
    bundle_id: String,
    bundle: ProjectBundle,
}

pub(crate) struct ResumeRepositoryPreflightApply {
    pub(crate) preflight: ResumeRepositorySourcePreflight,
}

pub(crate) struct LifecycleReload {
    pub(crate) update: DashboardLifecycleUpdate,
    pub(crate) operation: Option<ActiveLifecycleOperation>,
}

pub(crate) struct LifecycleReloaded {
    reload: LifecycleReload,
    result: std::result::Result<DashboardMetadata, String>,
}

impl From<mj_controller::controller::NewSessionPreflight> for RemotePreflightOutcome {
    fn from(result: mj_controller::controller::NewSessionPreflight) -> Self {
        if !result.remote_repairs.is_empty() {
            return Self::Repair(result.remote_repairs);
        }
        Self::Ready(
            result
                .remote_repositories
                .into_iter()
                .map(|repository| RemoteRepositoryPreview {
                    repository_id: repository.id,
                    fetch_url: repository.fetch_url,
                    default_branch: repository.default_branch,
                    push_urls: repository.push_urls,
                })
                .collect(),
        )
    }
}

/// Preferences absent from the runtime feed. A background completion cannot
/// carry config or session records back into the dashboard's authoritative view.
pub(crate) struct DashboardMetadata {
    mount_history: mj_core::snapshot_map::SnapshotMap<String, Vec<PathBuf>>,
    container_sizes: mj_core::snapshot_map::SnapshotMap<String, mj_core::state::HostContainerSize>,
}

impl From<Controller> for DashboardMetadata {
    fn from(controller: Controller) -> Self {
        Self {
            mount_history: controller.state.mount_history,
            container_sizes: controller.state.container_sizes,
        }
    }
}

impl DashboardMetadata {
    fn apply(self, state: &mut State) {
        state.mount_history = self.mount_history;
        state.container_sizes = self.container_sizes;
    }
}

/// Completed mutations wait for their referenced objects in the runtime feed.
/// They never install the independent snapshots used to perform the mutation.
pub(crate) struct PendingRuntimeUpdate {
    pub(crate) update: DashboardIoUpdate,
    pub(crate) deadline: std::time::Instant,
}

impl DashboardIoUpdate {
    pub(crate) fn awaiting_runtime(&self, controller: &Controller) -> bool {
        match self {
            Self::CreatedBundle { result } => result.as_ref().as_ref().is_ok_and(|created| {
                controller.config.bundles.get(&created.bundle_id) != Some(&created.bundle)
            }),
            Self::RemovedBundle { bundle_id, result } => {
                result.is_ok() && controller.config.bundles.contains_key(bundle_id)
            }
            // An imported session is stopped, and the feed carries only live
            // sessions; only the project it may have added arrives there.
            Self::ImportedSessionApplied { result } => {
                result.as_ref().as_ref().is_ok_and(|applied| {
                    controller.config.bundles.get(&applied.session.bundle_id)
                        != Some(&applied.bundle)
                })
            }
            Self::CreateSession(update) => matches!(update.as_ref(),
                DashboardCreateSessionUpdate::Registered(registered)
                    if !controller.state.sessions.contains_key(&registered.session.id)),
            _ => false,
        }
    }

    pub(crate) fn runtime_wait_expired(self) -> Self {
        let error = "The operation was saved, but the runtime feed has not confirmed it. Reconnect to refresh the view before retrying".to_owned();
        match self {
            Self::CreatedBundle { .. } => Self::CreatedBundle {
                result: Box::new(Err(error)),
            },
            Self::RemovedBundle { bundle_id, .. } => Self::RemovedBundle {
                bundle_id,
                result: Err(error),
            },
            Self::ImportedSessionApplied { .. } => Self::ImportedSessionApplied {
                result: Box::new(Err(error)),
            },
            Self::CreateSession(_) => Self::RuntimeConfirmationMissing(error),
            _ => unreachable!("only reference-dependent results wait for runtime state"),
        }
    }
}

impl DashboardContext {
    /// How a notice names a session: the title the session list shows, or
    /// the short id when the session has no title or its record is gone
    /// (launch findings B-3 and R5-5).
    pub(crate) fn session_notice_name(&self, session_id: &str) -> String {
        session_notice_name(&self.controller.state, session_id)
    }

    fn apply_controller_metadata(&mut self, metadata: DashboardMetadata) {
        metadata.apply(&mut self.controller.state);
    }

    /// Folds one finished background job into dashboard and controller state.
    pub(super) fn apply_dashboard_io_update(&mut self, update: DashboardIoUpdate) {
        let project_context = match &update {
            DashboardIoUpdate::ProjectCatalog { context, .. }
            | DashboardIoUpdate::MountHistory { context, .. } => Some(context),
            _ => None,
        };
        if project_context
            .is_some_and(|context| *context != self.dashboard.project_catalog_context())
        {
            return;
        }
        if update.awaiting_runtime(&self.controller) {
            self.dashboard
                .set_notice("Saved; waiting for the runtime view…");
            self.pending_runtime_updates.push(PendingRuntimeUpdate {
                update,
                deadline: std::time::Instant::now() + SAVE_ACK_TIMEOUT,
            });
            return;
        }
        match update {
            DashboardIoUpdate::RuntimeConfirmationMissing(error) => {
                self.dashboard.set_failure_notice(error)
            }
            DashboardIoUpdate::HelpSearchFinished { generation, result } => {
                self.dashboard.apply_help_search_result(generation, result);
            }
            DashboardIoUpdate::WorkspacePaneSizes { result } => match result {
                Ok(layouts) => {
                    for (workspace_id, sizes) in layouts {
                        if !self.known_workspace_layouts.contains(&workspace_id) {
                            continue;
                        }
                        self.dashboard
                            .cache_workspace_pane_sizes(&workspace_id, sizes);
                        self.pane_size_persistence.remember(workspace_id, sizes);
                    }
                }
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not load workspace pane sizes: {error}")),
            },
            DashboardIoUpdate::WorkspaceLayouts { result } => match result {
                Ok(layouts) => {
                    for (workspace_id, layout) in layouts {
                        if !self.known_workspace_layouts.contains(&workspace_id) {
                            continue;
                        }
                        self.workspace_layouts
                            .insert(workspace_id.clone(), layout.clone());
                        self.dashboard
                            .cache_workspace_layout(&workspace_id, layout.clone());
                        self.layout_persistence.remember(workspace_id, layout);
                    }
                }
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not load workspace layouts: {error}")),
            },
            DashboardIoUpdate::WorkspaceClosed {
                generation,
                workspace_id,
                result,
            } => {
                let generation = self
                    .dashboard
                    .workspace_close_finished(&workspace_id)
                    .unwrap_or(generation);
                let deleted = result.is_ok();
                let result = result.map(|()| WorkspaceManagementResult {
                    entries: self.dashboard.workspace_entries_without(&workspace_id),
                    select_workspace: None,
                    deleted_workspace_id: Some(workspace_id),
                });
                self.apply_dashboard_io_update(DashboardIoUpdate::WorkspaceManagement {
                    generation,
                    result,
                });
                if deleted {
                    self.dashboard.set_notice("Workspace deleted.");
                }
            }
            DashboardIoUpdate::WorkspaceCloseCancelled { result } => {
                self.dashboard.set_notice(match result {
                    Ok(()) => {
                        "Workspace deletion cancellation requested; completed suspensions cannot be undone."
                            .into()
                    }
                    Err(error) => format!("Could not cancel workspace close: {error}"),
                });
            }
            DashboardIoUpdate::WorkspaceManagement { generation, result } => match result {
                Ok(result) => {
                    let WorkspaceManagementResult {
                        entries,
                        select_workspace,
                        deleted_workspace_id,
                    } = result;
                    // The runtime feed alone owns tab names, layouts and the
                    // active tab. A deletion owns only the composers, because
                    // the feed also hides an empty default it cannot tell
                    // apart from a deleted one.
                    if let Some(deleted_workspace_id) = deleted_workspace_id.as_deref() {
                        self.discard_workspace_composers(deleted_workspace_id);
                        if self.pending_workspace_selection.as_deref() == Some(deleted_workspace_id)
                        {
                            self.pending_workspace_selection = None;
                        }
                    }
                    // The TUI owns the modal generation guard. It returns
                    // whether this result still belongs to the visible
                    // manager, so a late create cannot switch another tab.
                    let outcome = self
                        .dashboard
                        .finish_workspace_management(generation, Ok(entries));
                    let foreground = outcome.foreground;
                    if let DashboardAction::CloseWorkspace {
                        generation,
                        workspace_id,
                    } = outcome.action
                    {
                        spawn_workspace_close(
                            generation,
                            workspace_id,
                            self.dashboard_io_tx.clone(),
                        );
                    }
                    if foreground && let Some(workspace_id) = select_workspace {
                        self.dashboard.cancel_modal();
                        if self.known_workspace_layouts.contains(&workspace_id) {
                            self.select_workspace(Some(workspace_id));
                        } else {
                            // The tab opens with the feed frame that adds it.
                            self.pending_workspace_selection = Some(workspace_id);
                        }
                    }
                }
                Err(error) => {
                    if !self
                        .dashboard
                        .finish_workspace_management(generation, Err(error.clone()))
                        .foreground
                    {
                        self.dashboard
                            .set_notice(format!("Workspace operation failed: {error}"));
                    }
                }
            },
            DashboardIoUpdate::NativeAgentHistory {
                owner,
                child,
                result,
            } => {
                self.dashboard
                    .native_agent_history_loaded(&owner, &child, result);
            }
            DashboardIoUpdate::NativeAgentStopped {
                owner,
                child,
                result,
            } => {
                self.dashboard
                    .native_agent_stop_finished(&owner, &child, result);
            }
            DashboardIoUpdate::InterruptAllFinished { targets, failures } => {
                self.dashboard.interrupt_all_finished(&targets, failures);
            }
            DashboardIoUpdate::ReviewRefused {
                session_id,
                message,
            } => {
                match self.chats.get_mut(&session_id) {
                    Some(chat) => chat.report_review_refusal(message),
                    // The chat moved on; the refusal still belongs on screen.
                    None => self.dashboard.set_notice(message),
                }
            }
            DashboardIoUpdate::WorkerRecordPersistence { operation, result } => {
                match (operation, result) {
                    (WorkerRecordPersistence::AcpTitle { .. }, Err(error)) => self
                        .dashboard
                        .set_notice(format!("Could not save harness title: {error}")),
                    (
                        WorkerRecordPersistence::TargetMissing {
                            session_id,
                            detail: _,
                            updated_at: _,
                        },
                        Ok(WorkerRecordPersistenceOutcome::TargetMissing(state)),
                    ) => {
                        let name = self.session_notice_name(&session_id);
                        let notice = match state {
                            SessionState::Error => format!(
                                "Session {name} cannot reach its managed target; its last verified checkpoint is ready to resume"
                            ),
                            SessionState::Lost => format!("Session {name} lost its managed target"),
                            _ => unreachable!("target-missing result is error or lost"),
                        };
                        self.dashboard.set_notice(notice);
                    }
                    (WorkerRecordPersistence::TargetMissing { session_id, .. }, Err(error)) => {
                        self.dashboard.set_notice(format!(
                            "Could not record missing target for {}: {error}",
                            self.session_notice_name(&session_id)
                        ))
                    }
                    (
                        WorkerRecordPersistence::AcpTitle { .. },
                        Ok(WorkerRecordPersistenceOutcome::Saved),
                    )
                    | (
                        WorkerRecordPersistence::TargetMissing { .. },
                        Ok(WorkerRecordPersistenceOutcome::Unchanged),
                    ) => {}
                    (operation, Ok(outcome)) => {
                        unreachable!("persistence operation {operation:?} returned {outcome:?}")
                    }
                }
            }
            DashboardIoUpdate::MaterializedSessionProjection { session_id, result } => {
                self.finish_materialized_projection(session_id, result);
            }
            DashboardIoUpdate::TranscriptTailSeed { materialized } => {
                let viewed_through_event_ordinal = self
                    .controller
                    .state
                    .sessions
                    .get(&materialized.session_id)
                    .map_or(0, |session| session.viewed_through_event_ordinal);
                self.request_materialized_projection(*materialized, viewed_through_event_ordinal);
            }
            DashboardIoUpdate::StoppedSubagentTranscript { session_id, result } => {
                self.dashboard
                    .set_stopped_subagent_transcript(&session_id, result);
            }
            DashboardIoUpdate::StoredSessionSummary { session_id, result } => {
                match result {
                    Ok(summary) => {
                        self.dashboard
                            .apply_prepared_materialized_session_summary(summary);
                    }
                    Err(error) => tracing::warn!(
                        %session_id,
                        "could not restore stored session summary: {error}"
                    ),
                }
                // Either way this session has answered, so the startup pick is
                // one summary closer to being able to choose.
                self.finish_startup_summary(&session_id);
            }
            DashboardIoUpdate::ProjectSource { session_id, result } => {
                self.project_sources_in_flight.remove(&session_id);
                match result {
                    Ok(source) => self.dashboard.set_project_source(&session_id, source),
                    Err(error) => tracing::warn!(
                        %session_id,
                        "could not resolve canonical project source: {error}"
                    ),
                }
            }
            DashboardIoUpdate::ChatOpened {
                generation,
                assignment,
                pane,
                session_id,
                result,
            } => {
                // Ignore a late result after the pane that asked for it moved
                // on (or the dashboard has shut down).
                if !self.attachments.get(&pane).is_some_and(|attachment| {
                    attachment.accepts_pane_result(
                        generation,
                        assignment,
                        &self.dashboard,
                        pane,
                        &session_id,
                    )
                }) || self.opening_chat_sessions.get(&pane).map(String::as_str)
                    != Some(session_id.as_str())
                {
                    return;
                }
                self.opening_chat_sessions.remove(&pane);
                self.sync_opening_session();
                // The attach may have crossed a lifecycle boundary while it
                // was preparing. Keep the warm chat/draft untouched and drop
                // the late result instead of reviving a retiring conversation.
                if self.dashboard.transition_kind(&session_id).is_some()
                    || self
                        .dashboard
                        .transition_failure_kind(&session_id)
                        .is_some()
                {
                    self.defer_chat_open_in(pane);
                    return;
                }
                match *result {
                    Ok(chat) => {
                        // A chat already warm for this session continued
                        // receiving feed updates while the attach was in
                        // flight. Capture and persist its latest local
                        // composer just before replacing it.
                        self.record_chat_detach(&session_id);
                        self.save_question_draft(&session_id);
                        // Whatever the user typed into the standby composer
                        // while this attach ran is the newest draft, so it
                        // wins over the copy captured when the open started.
                        let chat = if let Some(text) =
                            self.dashboard.take_standby_prompt_draft(&session_id)
                        {
                            chat.with_draft(text)
                        } else if let Some(draft) = self.composer_drafts.get(&session_id) {
                            chat.with_draft(draft.text.clone())
                        } else {
                            chat
                        };
                        let mut chat = chat.open_replacing(self.chats.get(&session_id));
                        if let Some(position) = self.transcript_positions.get(&session_id) {
                            chat.restore_transcript_position(position.clone());
                        }
                        self.restore_question_draft(&session_id, &mut chat);
                        self.chats.insert(session_id.clone(), chat);
                        // The context travelled with the attach, which is
                        // asynchronous; anything the surface learned while it
                        // was in flight is handed over now.
                        self.refresh_chat_context();
                        self.apply_runtime_review_to_chat(&session_id);
                        self.dashboard.clear_notice();
                        self.acknowledge_visible_chats();
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, %error, "could not open session");
                        self.dashboard
                            .report_open_failure(&session_id, &error.to_string());
                    }
                }
            }
            DashboardIoUpdate::DaemonRestarted(result) => match result {
                Ok(sentence) => self.dashboard.set_notice(sentence),
                Err(error) => self.dashboard.set_failure_notice(error),
            },
            DashboardIoUpdate::GoSelectionSaved(result) => {
                self.go_selection_in_flight = false;
                if let Err(error) = result {
                    self.dashboard.set_failure_notice(format!(
                        "Could not remember this conversation: {error}"
                    ));
                }
            }
            DashboardIoUpdate::GitStatus { session_id, result } => {
                self.git_probes_in_flight.remove(&session_id);
                self.dashboard.set_git_status(session_id, result);
            }
            DashboardIoUpdate::GoContext { session_id, result } => {
                self.go_context_in_flight = false;
                self.dashboard.set_go_context(session_id, result);
            }
            DashboardIoUpdate::GoPrepared {
                workspace_id,
                retry,
                result,
            } => match *result {
                Ok((_config, recipe)) => {
                    let action = self.dashboard.go_launch_action(workspace_id, recipe);
                    super::actions::start_session_launch(self, action);
                }
                Err(error) => {
                    if self.dashboard.active_workspace_id() == Some(workspace_id.as_str()) {
                        self.dashboard.show_launch_failure(error, Some(*retry));
                    } else {
                        self.dashboard.set_failure_notice(format!("Session preparation failed in workspace {workspace_id}: {error}. Return to that workspace to retry."));
                    }
                }
            },
            DashboardIoUpdate::CreateSession(update) => self.apply_create_session_update(*update),
            DashboardIoUpdate::RemotePreflight {
                generation,
                launch,
                result,
            } => {
                if generation == self.dashboard.session_preflight_generation() {
                    match result {
                        Ok(RemotePreflightOutcome::Repair(repairs)) => {
                            self.dashboard.apply_remote_session_preflight(
                                generation,
                                Err("Git tracking needs repair".into()),
                            );
                            if let DashboardAction::CreateSession { bundle_id, .. } = &*launch {
                                self.dashboard.show_remote_repair_confirmation(
                                    bundle_id.clone(),
                                    repairs,
                                    DashboardAction::PreflightCreateSession { launch },
                                );
                            }
                        }
                        other => self.dashboard.apply_remote_session_preflight(
                            generation,
                            other.map(|outcome| match outcome {
                                RemotePreflightOutcome::Ready(repositories) => repositories,
                                RemotePreflightOutcome::Repair(_) => unreachable!(),
                            }),
                        ),
                    }
                }
            }
            DashboardIoUpdate::RepositoryRemotesRepaired {
                generation,
                retry,
                result,
            } => {
                if generation.is_some_and(|generation| {
                    generation != self.dashboard.session_preflight_generation()
                }) {
                    return;
                }
                self.dashboard.clear_notice();
                match result {
                    Ok(()) => match *retry {
                        DashboardAction::PreflightCreateSession { launch } => {
                            super::actions::start_create_session_preflight(self, *launch)
                        }
                        action => super::actions::start_session_launch(self, action),
                    },
                    Err(error) => {
                        if let Some(generation) = generation {
                            self.dashboard
                                .apply_remote_session_preflight(generation, Err(error.clone()));
                        }
                        self.dashboard.show_launch_failure(error, Some(*retry));
                    }
                }
            }
            DashboardIoUpdate::StartupPromptQueued {
                session_id,
                text,
                result,
            } => {
                // A queued prompt shows as a preview already, so success needs
                // no notice. A refusal puts the text back in the composer.
                if let Err(error) = result {
                    self.dashboard.restore_standby_prompt(&session_id, &text);
                    self.dashboard.set_failure_notice(format!(
                        "Could not queue the prompt for session {}: {error}",
                        self.session_notice_name(&session_id)
                    ));
                }
            }
            DashboardIoUpdate::StartupPromptWithdrawn {
                session_id,
                text,
                result,
            } => {
                let name = self.session_notice_name(&session_id);
                match result {
                    Ok(true) => {}
                    Ok(false) => {
                        let dropped = self.dashboard.standby_prompt_was_sent(&session_id, &text);
                        self.dashboard.set_notice(if dropped {
                            format!(
                                "That prompt had already been sent to session {name}, so it is not in the composer any more."
                            )
                        } else {
                            format!(
                                "That prompt had already been sent to session {name}; the composer still holds your edit, and Enter sends it as a new prompt."
                            )
                        });
                    }
                    Err(error) => {
                        self.dashboard
                            .standby_prompt_withdrawal_unconfirmed(&session_id, &text);
                        self.dashboard.set_failure_notice(format!(
                            "Could not take the prompt back from session {name} ({error}); it is still queued and will be sent when the session is live."
                        ));
                    }
                }
            }
            DashboardIoUpdate::RenameSession { title, result } => match result {
                Ok(title) => {
                    self.dashboard.set_notice(format!(
                        "Renamed session to {}",
                        mj_tui::fit_session_name(&title, mj_tui::NOTICE_NAME_CELLS)
                    ));
                }
                Err(error) => {
                    self.dashboard.set_notice(format!(
                        "Rename failed for {}: {error}",
                        mj_tui::fit_session_name(&title, mj_tui::NOTICE_NAME_CELLS)
                    ));
                }
            },
            DashboardIoUpdate::ChangeWorkspace {
                session_id,
                workspace_name,
                result,
            } => match result {
                Ok(()) => self.dashboard.set_notice(format!(
                    "Moved {} to workspace \"{}\".",
                    self.session_notice_name(&session_id),
                    mj_tui::fit_session_name(&workspace_name, mj_tui::NOTICE_NAME_CELLS)
                )),
                Err(error) => self.dashboard.set_failure_notice(format!(
                    "Could not move {} to workspace \"{}\": {error}",
                    self.session_notice_name(&session_id),
                    mj_tui::fit_session_name(&workspace_name, mj_tui::NOTICE_NAME_CELLS)
                )),
            },
            DashboardIoUpdate::ContainerSettings { session_id, result } => match result {
                Ok(controller) => {
                    self.apply_controller_metadata(controller);
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.refresh_chat_context();
                    self.dashboard.set_notice(format!(
                        "Container settings saved for {}; applies when it is next recreated.",
                        self.session_notice_name(&session_id)
                    ));
                }
                Err(error) => self.dashboard.set_notice(format!(
                    "Container settings failed for {}: {error}",
                    self.session_notice_name(&session_id)
                )),
            },
            DashboardIoUpdate::ProjectCatalog { result, .. } => match result {
                Ok(view) => {
                    let mut history = self.controller.state.mount_history.clone();
                    for location in view.locations {
                        let paths = history
                            .entry(format!("project:{}", location.host))
                            .or_insert_with(Vec::new);
                        if !paths.contains(&location.checkout_root) {
                            paths.push(location.checkout_root);
                        }
                    }
                    self.controller.state.mount_history = history.clone();
                    self.dashboard.apply_mount_history(history);
                    self.dashboard
                        .apply_project_catalog_status(view.status.clone());
                    if let Some(summary) = view.status.failure_summary() {
                        self.dashboard.set_notice(summary);
                    } else {
                        self.dashboard.set_notice("Recent projects refreshed.");
                    }
                }
                Err(error) => {
                    self.dashboard.apply_project_catalog_status(
                        mj_core::project_catalog::ProjectCatalogStatus::Failed {
                            errors: vec![error.clone()],
                        },
                    );
                    self.dashboard
                        .set_notice(format!("Project discovery failed: {error}"));
                }
            },
            DashboardIoUpdate::MountHistory { result, .. } => match result {
                Ok(history) => {
                    // The controller copy is what later `set_state` calls
                    // publish, so it has to carry the fresh history too.
                    self.controller.state.mount_history = history.clone().into_iter().collect();
                    self.dashboard.apply_mount_history(history);
                }
                Err(error) => {
                    tracing::warn!(%error, "could not refresh recent project directories");
                }
            },
            DashboardIoUpdate::TargetReadiness {
                generation,
                target_id,
                result,
                absent_engine,
            } => {
                if let (Some(template), Err(message)) = (&absent_engine, &result)
                    && self
                        .absent_engines
                        .record(&target_id, template, message.clone())
                {
                    tracing::info!(
                        target_id,
                        reason = %message,
                        "the target's container engine is not installed; the dashboard checks it \
                         again only when its configuration changes or the engine is installed"
                    );
                }
                match (&absent_engine, result) {
                    (Some(_), Err(message)) => self
                        .dashboard
                        .apply_target_runtime_missing(generation, target_id, message),
                    (_, result) => self
                        .dashboard
                        .apply_target_readiness(generation, target_id, result),
                }
            }
            DashboardIoUpdate::ProjectDirectory { path, exists } => {
                self.dashboard.apply_project_directory_check(path, exists);
            }
            DashboardIoUpdate::TargetTest { target_id, result } => {
                self.target_test_cancel = None;
                self.dashboard.apply_target_test(target_id, result);
            }
            DashboardIoUpdate::ConfigRename { what, result } => match result {
                Ok(controller) => {
                    self.apply_controller_metadata(controller);
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.refresh_chat_context();
                    // The daemon renamed the profile, saw the profile set
                    // change and probes the new id once; nothing to ask here.
                    self.dashboard.set_notice(format!("Renamed {what}."));
                }
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not rename {what}: {error}")),
            },
            DashboardIoUpdate::QuotaRefreshRequested(Ok(())) => {}
            DashboardIoUpdate::QuotaRefreshRequested(Err(error)) => {
                self.manual_quota_refresh_cycles = None;
                self.dashboard
                    .set_notice(format!("Could not refresh quota: {error}"));
            }
            DashboardIoUpdate::WebAccess { generation, access } => {
                if generation == self.web_request_generation {
                    self.dashboard.apply_web_access(access);
                }
            }
            DashboardIoUpdate::ResumeCandidates {
                discovery_id,
                result,
            } => {
                if self
                    .dashboard
                    .apply_resume_candidates(discovery_id, *result)
                {
                    // Checkpoint sizes follow the stopped records.
                    self.controller_changed = true;
                }
            }
            DashboardIoUpdate::ResumeRecord { session_id, result } => {
                match self.dashboard.apply_resume_record(&session_id, *result) {
                    DashboardAction::None => {}
                    DashboardAction::ResolveAwsResourceOptions {
                        target_template_ids,
                    } => self.resolve_aws_resource_options(target_template_ids),
                    action => tracing::warn!(
                        ?action,
                        "the resume wizard asked for an action its record loader cannot run"
                    ),
                }
            }
            DashboardIoUpdate::WikiRows { request_id, result } => {
                if self.dashboard.apply_wiki_search_result(request_id, result) {
                    self.load_resume_preview();
                }
            }
            DashboardIoUpdate::ResumeTextMatches { request_id, result } => {
                if self
                    .dashboard
                    .apply_resume_text_search_result(request_id, result)
                {
                    self.load_resume_preview();
                }
            }
            DashboardIoUpdate::SessionTextMatches { request_id, result } => {
                self.dashboard.apply_sessions_text(request_id, result);
            }
            DashboardIoUpdate::WikiBrief { wiki_id, result } => match result {
                Ok(markdown) => self.dashboard.apply_wiki_brief(wiki_id, markdown),
                Err(error) => self
                    .dashboard
                    .apply_wiki_brief(wiki_id, format!("Could not load the transcript: {error}")),
            },
            DashboardIoUpdate::WikiHits {
                wiki_id,
                query,
                result,
            } => match result {
                Ok(transcript) => self.dashboard.apply_wiki_hits(wiki_id, query, transcript),
                // Shown in the pane the same way a failed briefing is, so the
                // reason sits where the passages were promised.
                Err(error) => self.dashboard.apply_wiki_hits(
                    wiki_id,
                    query,
                    Some(mj_client::daemon::WikiHitTranscript {
                        blocks: vec![mj_client::daemon::WikiHitBlock {
                            text: format!("Could not load the matching passages: {error}"),
                            ..Default::default()
                        }],
                        omitted_after: 0,
                    }),
                ),
            },
            DashboardIoUpdate::WebListeners { generation, result } => {
                if generation == self.web_request_generation {
                    self.dashboard.apply_web_listeners(result);
                }
            }
            DashboardIoUpdate::WebAccessError { generation, error } => {
                if generation == self.web_request_generation {
                    self.dashboard.apply_web_error(error);
                }
            }
            DashboardIoUpdate::BuildCachePreviewed {
                generation,
                key,
                result,
                install_mbx_available,
            } => self.dashboard.build_cache_previewed(
                generation,
                &key,
                result,
                install_mbx_available,
            ),
            DashboardIoUpdate::MbxInstalled {
                generation,
                key,
                machine_id,
                result,
                preview,
                install_mbx_available,
            } => self.dashboard.mbx_install_finished(
                generation,
                &key,
                &machine_id,
                result,
                preview,
                install_mbx_available,
            ),
            DashboardIoUpdate::ArchiveSpacePreviewed {
                generation,
                older_than_days,
                result,
            } => self
                .dashboard
                .archive_space_previewed(generation, older_than_days, result),
            DashboardIoUpdate::SetupDiscovered { generation, result } => {
                self.dashboard.setup_discovered(generation, result)
            }
            DashboardIoUpdate::FirstRunStarted => self.dashboard.begin_welcome(),
            DashboardIoUpdate::FirstRunConfigured(report) => {
                self.dashboard.welcome_configured(report.summary())
            }
            DashboardIoUpdate::FirstRunChecked(result) => match result {
                Ok(Some(errors)) => self.dashboard.welcome_checked(errors),
                Ok(None) => {}
                Err(error) => self.dashboard.welcome_checked(vec![format!(
                    "Setup could not finish: {error}. Run `mj setup` to retry."
                )]),
            },
            DashboardIoUpdate::SetupSaved { generation, result } => {
                let result = result.map(Config::with_local_targets);
                // The acknowledgement closes the editor; only the runtime feed
                // installs configuration, including edits from other clients.
                let result = result.map(|_| self.controller.config.clone());
                self.dashboard.setup_saved(generation, result);
            }
            DashboardIoUpdate::ReviewSettingsChoices {
                generation,
                profile_id,
                model,
                choices,
            } => {
                self.dashboard.apply_review_settings_choices(
                    generation,
                    &profile_id,
                    model.as_deref(),
                    review_settings_choices(choices),
                );
            }
            DashboardIoUpdate::ReviewSettingsDiscovered {
                generation,
                profile_id,
                model,
                result,
            } => {
                if self.dashboard.apply_review_settings_discovery(
                    generation,
                    &profile_id,
                    model.as_deref(),
                    result,
                ) {
                    self.review_discovery_cancel = None;
                }
            }
            DashboardIoUpdate::SpinnerStyleSaved { result } => match result {
                Ok(config) => {
                    let style = config.spinner;
                    self.dashboard.finish_spinner_style_save();
                    let palette = self
                        .dashboard
                        .first_key_label(mj_tui::CommandId::Palette)
                        .map(|key| format!("{key} → "))
                        .unwrap_or_default();
                    self.dashboard.set_notice(format!(
                        "Spinner: {style}. {palette}Next spinner style to change it."
                    ));
                }
                Err(error) => {
                    self.dashboard.finish_spinner_style_save();
                    self.dashboard
                        .set_failure_notice(format!("Could not save spinner style: {error}"));
                }
            },
            DashboardIoUpdate::ClipboardWritten(result) => {
                if let Err(error) = result {
                    self.dashboard.set_failure_notice(format!(
                        "Copy to the system clipboard failed: {error}"
                    ));
                }
            }
            DashboardIoUpdate::ClipboardText(result) => {
                self.clipboard_read_in_flight = false;
                match result {
                    Ok(text) => self.dashboard.handle_paste(&text),
                    Err(error) => self.dashboard.set_notice(format!("Paste failed: {error}")),
                }
            }
            DashboardIoUpdate::DetachedSessionState { session_id, result } => match result {
                Ok(()) => {
                    self.draft_save_failures.remove(&session_id);
                }
                Err(error) => {
                    let message = format!(
                        "Could not confirm saved draft and read status for {}: {error}",
                        self.session_notice_name(&session_id)
                    );
                    self.draft_save_failures.insert(session_id, message.clone());
                    self.dashboard.set_notice(message);
                }
            },
            DashboardIoUpdate::ReadReceipt { session_id, result } => {
                self.finish_read_receipt(session_id, result);
            }
            DashboardIoUpdate::CreatedBundle { result } => match *result {
                Ok(created) => {
                    let followup = self
                        .dashboard
                        .apply_created_bundle(self.controller.config.clone(), &created.bundle_id);
                    if let DashboardAction::ResolveAwsResourceOptions {
                        target_template_ids,
                    } = followup
                    {
                        self.resolve_aws_resource_options(target_template_ids);
                    }
                }
                Err(error) => {
                    self.dashboard.fail_bundle_creation(&error);
                }
            },
            DashboardIoUpdate::RemovedBundle { bundle_id, result } => match result {
                Ok(()) => self
                    .dashboard
                    .apply_removed_bundle(self.controller.config.clone(), &bundle_id),
                Err(error) => self.dashboard.fail_bundle_removal(&error),
            },
            DashboardIoUpdate::ImportedSessionApplied { result } => match *result {
                Ok(applied) => {
                    let session_id = applied.session.id.clone();
                    // The import's own answer carries the stopped record the
                    // resume wizard opens on.
                    self.dashboard
                        .remember_stopped_record(applied.session.clone());
                    self.resolve_project_sources();
                    self.dashboard.set_notice(format!(
                        "Imported {} session {}.",
                        applied.harness, applied.native_session_id
                    ));
                    // Import completion can arrive after a tab switch. Keep
                    // the imported record globally visible, but only open
                    // the resume modal in the workspace that owns it.
                    if self.session_in_active_workspace(&session_id)
                        && let DashboardAction::ResolveAwsResourceOptions {
                            target_template_ids,
                        } = self.dashboard.begin_resume_for(&session_id)
                    {
                        self.resolve_aws_resource_options(target_template_ids);
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "Saving imported session failed");
                    self.dashboard
                        .set_failure_notice(format!("Import failed: {error}"));
                }
            },
            DashboardIoUpdate::LifecycleReloaded(reloaded) => {
                self.apply_lifecycle_reloaded(*reloaded)
            }
            DashboardIoUpdate::LifecycleCancellation { session_id, result } => {
                if let Err(error) = result {
                    self.dashboard.set_failure_notice(format!(
                        "Could not cancel operation for {}: {error}",
                        self.session_notice_name(&session_id)
                    ));
                }
            }
            DashboardIoUpdate::MovePrepared {
                session_id,
                request_id,
                result,
            } => match result {
                Ok(preparation) => {
                    self.dashboard
                        .apply_move_preparation(request_id, preparation);
                }
                Err(error) => {
                    self.dashboard
                        .set_move_preparation_failed(&session_id, request_id, error);
                }
            },
            DashboardIoUpdate::CheckpointArchiveSizes {
                generation,
                targets,
                sizes,
            } => {
                self.dashboard.patch_checkpoint_archive_sizes(
                    sizes
                        .into_iter()
                        .filter(|(id, _)| {
                            targets.get(id) == self.checkpoint_archive_targets_seen.get(id)
                                && self.checkpoint_archive_pending.get(id) == Some(&generation)
                        })
                        .collect(),
                );
            }
            DashboardIoUpdate::WorkerDiagnosis {
                session_id,
                episode_id,
                result,
            } => self.apply_worker_diagnosis(session_id, episode_id, result),
            DashboardIoUpdate::ProfileHydration { key, result } => {
                self.dashboard.apply_profile_hydration(key, result)
            }
            DashboardIoUpdate::ProjectDiscovery { context, result } => {
                self.dashboard.apply_project_discovery(&context, result);
            }
            DashboardIoUpdate::PathCompletions {
                context,
                prefix,
                result,
            } => match result {
                Ok(completion) => self
                    .dashboard
                    .apply_path_completions(&context, &prefix, completion),
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Path completion failed: {error}")),
            },
            DashboardIoUpdate::MountValidation {
                context,
                source,
                result,
            } => {
                let action = self
                    .dashboard
                    .apply_resolved_mount_source(&context, &source, result);
                super::actions::start_move_preparation(self, action);
            }
            DashboardIoUpdate::SessionMountValidation {
                generation,
                launch,
                result,
            } => {
                if generation != self.dashboard.session_preflight_generation() {
                    if let Err(error) = result {
                        tracing::warn!(%error, "cancelled mount preflight failed");
                    }
                    return;
                }
                match result {
                    Ok(None) => match *launch {
                        DashboardAction::PreflightResumeRepositories { launch } => {
                            if let Err(error) =
                                super::actions::start_resume_repository_preflight(self, launch)
                            {
                                self.dashboard.fail_resume_preflight(format!(
                                    "Could not check checkpoint repositories: {error:#}"
                                ));
                            }
                        }
                        launch => {
                            if matches!(&launch, DashboardAction::CreateSession { .. }) {
                                super::actions::start_create_session_preflight(self, launch);
                            } else {
                                self.dashboard.finish_session_mount_preflight();
                                super::actions::start_session_launch(self, launch);
                            }
                        }
                    },
                    Ok(Some((source, error))) => {
                        self.dashboard
                            .apply_session_mount_preflight_failure(&source, error);
                    }
                    Err(error) => {
                        let error = format!("Could not check attached directories: {error}");
                        self.dashboard
                            .apply_remote_session_preflight(generation, Err(error.clone()));
                        self.dashboard.fail_resume_preflight(error);
                    }
                }
            }
            DashboardIoUpdate::ResumeRepositoryPreflight {
                generation,
                launch,
                submitted_repository_id,
                result,
            } => {
                if generation != self.dashboard.session_preflight_generation() {
                    if let Err(error) = *result {
                        tracing::warn!(%error, "cancelled repository preflight failed");
                    }
                    return;
                }
                match *result {
                    Ok(applied) => match applied.preflight {
                        ResumeRepositorySourcePreflight::Ready(receipt) => {
                            self.dashboard.finish_resume_repository_preflight();
                            super::actions::start_preflighted_session_launch(
                                self, *launch, receipt,
                            );
                        }
                        ResumeRepositorySourcePreflight::ConvertingRawCheckout {
                            receipt,
                            preview,
                        } => {
                            self.dashboard
                                .show_raw_conversion_confirmation(*launch, receipt, *preview);
                        }
                        ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) => {
                            if submitted_repository_id.as_deref()
                                == Some(mismatch.repository_id.as_str())
                            {
                                self.dashboard.apply_repository_origin_failure(
                                    &mismatch.repository_id,
                                    format!(
                                        "That origin does not contain checkpoint base {}.",
                                        mismatch.missing_commit
                                    ),
                                );
                            } else {
                                self.dashboard.show_repository_origin_dialog(
                                    mismatch.session_id,
                                    mismatch.repository_id,
                                    mismatch.missing_commit,
                                    mismatch.archived_origin,
                                    mismatch.configured_origin,
                                    *launch,
                                );
                            }
                        }
                    },
                    Err(error) => {
                        if let Some(repository_id) = submitted_repository_id {
                            self.dashboard
                                .apply_repository_origin_failure(&repository_id, error);
                        } else {
                            self.dashboard.fail_resume_preflight(format!(
                                "Could not check checkpoint repositories: {error}"
                            ));
                        }
                    }
                }
            }
            DashboardIoUpdate::ContainerPathResolved { context, result } => {
                self.dashboard.container_path_resolved(&context, result)
            }
            DashboardIoUpdate::SetupPathResolved {
                generation,
                draft,
                path,
                value,
                result,
            } => self
                .dashboard
                .setup_path_resolved(generation, &draft, &path, &value, result),
            DashboardIoUpdate::ProjectValidation {
                context,
                directory,
                result,
            } => self
                .dashboard
                .apply_resolved_project_directory(&context, &directory, result),
        }
    }

    fn load_resume_preview(&mut self) {
        match self.dashboard.next_wiki_preview() {
            DashboardAction::LoadArchivedBrief { wiki_id } => {
                spawn_wiki_brief(wiki_id, self.dashboard_io_tx.clone());
            }
            DashboardAction::LoadArchivedHits { wiki_id, query } => {
                spawn_wiki_hits(wiki_id, query, self.dashboard_io_tx.clone());
            }
            _ => {}
        }
    }

    fn apply_create_session_update(&mut self, update: DashboardCreateSessionUpdate) {
        match update {
            DashboardCreateSessionUpdate::RemoteRepair {
                bundle_id,
                repairs,
                retry,
            } => {
                self.dashboard.clear_notice();
                self.dashboard
                    .show_remote_repair_confirmation(bundle_id, repairs, *retry);
            }
            DashboardCreateSessionUpdate::Registered(registered) => {
                let registered = *registered;
                let session_id = registered.session.id.clone();
                // A restore has its session now; the archive may be restored
                // again from here on.
                self.dashboard
                    .finish_archive_restore(&registered.retry_launch);
                if let Some((host, size)) = registered.remembered_container_size {
                    self.controller.state.remember_container_size(&host, size);
                }
                let activate = self.dashboard.launch_standby_capturing();
                if activate {
                    self.dashboard.select_active_session(&session_id);
                }
                self.resolve_project_sources();
                let still_launching = self
                    .controller
                    .state
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.state == SessionState::Provisioning);
                if still_launching {
                    self.dashboard.begin_session_operation(
                        session_id.clone(),
                        SessionOperationKind::Launching,
                        None,
                    );
                }
                // Anything typed while the launch was being prepared belongs to
                // this session now: its composer becomes the session's standby,
                // and each prompt already entered there goes to the daemon to
                // be delivered when the harness is ready.
                for text in self.dashboard.adopt_launch_standby(&session_id) {
                    spawn_startup_prompt(
                        session_id.clone(),
                        text,
                        None,
                        self.dashboard_io_tx.clone(),
                        self.critical_operations.clone(),
                    );
                }
                // The next thing the person does with a launching session is
                // write its first message, so the keyboard starts where the
                // type-ahead composer is.
                if activate {
                    self.dashboard.focus_prompt();
                }
                let notice_name = self.session_notice_name(&session_id);
                self.dashboard
                    .set_notice(format!("Launching {notice_name}…"));
                if still_launching {
                    self.lifecycle_operations.insert(
                        session_id,
                        ActiveLifecycleOperation {
                            retirement: None,
                            cancelled: registered.cancelled,
                            kind: SessionOperationKind::Launching,
                            retry_launch: Some(registered.retry_launch),
                            notice_name,
                        },
                    );
                }
            }
            DashboardCreateSessionUpdate::Failed {
                error,
                retry_launch,
            } => {
                self.dashboard.finish_archive_restore(&retry_launch);
                self.dashboard
                    .show_launch_failure(error, Some(*retry_launch));
            }
        }
    }

    fn apply_lifecycle_reloaded(&mut self, reloaded: LifecycleReloaded) {
        let LifecycleReload { update, operation } = reloaded.reload;
        let session_id = update.session_id;
        if self
            .lifecycle_operations
            .get(&session_id)
            .is_some_and(|current| !Arc::ptr_eq(&current.cancelled, &update.operation))
        {
            if let Err(error) = &update.result {
                tracing::warn!(%session_id, %error, "superseded lifecycle reload failed");
            }
            return;
        }
        // Taken before the reload: a destroy removes the record, and the
        // runtime snapshot may already have dropped it.
        let name = lifecycle_notice_name(
            &self.controller.state,
            &session_id,
            operation
                .as_ref()
                .map(|operation| operation.notice_name.as_str()),
        );
        let loaded = match reloaded.result {
            Ok(loaded) => loaded,
            Err(error) => {
                self.dashboard
                    .set_notice(format!("Could not reload completed operation: {error}"));
                return;
            }
        };
        self.apply_controller_metadata(loaded);
        self.dashboard.set_state(self.controller.state.clone());
        self.resolve_project_sources();
        // A lifecycle may finish after the user changed tabs. Its durable
        // record still belongs in the global controller, but completion must
        // not move the visible selection or replace another workspace's chat.
        let focus_session = self.session_in_active_workspace(&session_id);
        let retirement = operation
            .as_ref()
            .and_then(|operation| operation.retirement.as_ref());
        let owns_chat = retirement
            .is_some_and(|retirement| retirement.is_current(&self.chats, &self.attachments));
        if update.result.is_ok()
            && let Some(retirement) = retirement
        {
            self.drop_warm_chat_for(retirement);
        }
        match update.result {
            Ok(LifecycleSuccess::Created) => {
                if focus_session {
                    self.dashboard.finish_new_session(&session_id);
                }
                self.dashboard
                    .set_notice(format!("Session {} is ready", name));
                // No quota probe here. Creating, resuming or moving a session
                // reads the quota already held; only the explicit Refresh and
                // the scheduled poll ask the usage endpoint.
            }
            Ok(LifecycleSuccess::Resumed {
                profile_id,
                target_id,
            }) => {
                // Seed the conversation from the tail rather than from a
                // transcript shipped back through the daemon reply. The view
                // keeps `TAIL_SEED_ITEMS` and discards everything before it,
                // so reading the whole projection was work proportional to
                // history for a result that was thrown away.
                if focus_session && owns_chat {
                    self.request_transcript_tail_seed(&session_id);
                    self.open_chat_session(&session_id);
                }
                self.dashboard
                    .set_notice(format!("Resumed {} with {profile_id} on {target_id}", name));
            }
            Ok(LifecycleSuccess::Moved(outcome)) => {
                if focus_session && owns_chat {
                    self.request_transcript_tail_seed(&session_id);
                    self.open_chat_session(&session_id);
                }
                let destination = format!("{}/{}", outcome.profile_id, outcome.target_template_id);
                self.dashboard
                    .set_notice(if outcome.outcome == "unchanged" {
                        format!(
                            "Move of {} unchanged on {destination} (operation {})",
                            name, outcome.operation_id
                        )
                    } else {
                        format!(
                            "Moved {} to {destination}; ready and idle (operation {})",
                            name, outcome.operation_id
                        )
                    });
            }
            Ok(LifecycleSuccess::Closed) => {
                self.dashboard.set_notice(format!("Suspended {}", name));
            }
            Ok(LifecycleSuccess::ForceStopped) => self.dashboard.set_notice(format!(
                "Suspended {} using the confirmed recovery copy; newer changes discarded",
                name
            )),
            Ok(LifecycleSuccess::DestroyedStopped) => self
                .dashboard
                .set_notice(format!("Permanently destroyed suspended session {}", name)),
            Ok(LifecycleSuccess::ForceDestroyed) => self
                .dashboard
                .set_notice(format!("Permanently destroyed session {}", name)),
            Err(error) => {
                if operation
                    .as_ref()
                    .is_some_and(|operation| operation.kind == SessionOperationKind::Suspending)
                {
                    self.dashboard.show_close_failure(session_id.clone(), error);
                } else if operation
                    .as_ref()
                    .is_some_and(|operation| operation.kind == SessionOperationKind::Launching)
                {
                    let retry = operation
                        .and_then(|operation| operation.retry_launch)
                        .filter(|_| !self.controller.state.sessions.contains_key(&session_id));
                    self.dashboard.show_launch_failure(error, retry);
                } else {
                    let label = operation
                        .as_ref()
                        .map_or("Operation", |operation| operation.kind.label());
                    self.dashboard
                        .set_failure_notice(format!("{label} failed: {error}"));
                }
            }
        }
    }

    fn apply_worker_diagnosis(
        &mut self,
        session_id: String,
        episode_id: u64,
        result: std::result::Result<Option<String>, String>,
    ) {
        let completion = self.worker_diagnoses.finish(&session_id, episode_id);
        if let Some(error) = completion.display_error {
            let mut message = format!("relay unreachable: {error}");
            match &result {
                Ok(Some(diagnosis)) => {
                    message.push_str("; ");
                    message.push_str(diagnosis);
                }
                Ok(None) => {}
                Err(failure) => {
                    message.push_str("; worker diagnostics failed: ");
                    message.push_str(failure);
                }
            }
            tracing::warn!(%session_id, "{message}");
            self.dashboard
                .report_session_unreachable(&session_id, false);
        } else if let Err(error) = &result {
            tracing::warn!(%session_id, "stale worker diagnosis task failed: {error}");
        }
        if let Some(restart_episode) = completion.restart_episode {
            crate::pollers::spawn_worker_diagnosis(
                &self.controller,
                session_id,
                restart_episode,
                self.dashboard_io_tx.clone(),
                self.critical_operations.clone(),
            );
        }
    }
}

/// How a notice names a session; see [`State::session_notice_name`].
pub(crate) fn session_notice_name(state: &State, session_id: &str) -> String {
    // A session named by its initial prompt can have a title of many
    // paragraphs; a notice names it by the start, on one line.
    mj_tui::fit_session_name(
        &state.session_notice_name(session_id),
        mj_tui::NOTICE_NAME_CELLS,
    )
}

/// How a lifecycle's completion notice names its session. The daemon's
/// runtime snapshot can drop a destroyed session's record before the
/// lifecycle reload lands, so when the record is gone the name taken when
/// the operation began stands in (launch finding R5-4).
fn lifecycle_notice_name(state: &State, session_id: &str, taken_at_start: Option<&str>) -> String {
    match taken_at_start {
        Some(name) if !state.sessions.contains_key(session_id) => {
            mj_tui::fit_session_name(name, mj_tui::NOTICE_NAME_CELLS)
        }
        _ => session_notice_name(state, session_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_controller::controller::create_quick_bundle_in_config as create_quick_bundle;
    use mj_core::config::{HarnessKind, ProjectRepository};

    #[test]
    fn saved_bundle_waits_for_runtime_identity_without_installing_old_config() {
        let completion = DashboardIoUpdate::CreatedBundle {
            result: Box::new(Ok(CreatedBundleUpdate {
                bundle_id: "created".into(),
                bundle: ProjectBundle {
                    primary_repo: "created".into(),
                    repositories: vec![],
                },
            })),
        };
        let mut controller = Controller {
            config: Config::default(),
            state: State::default(),
        };
        assert!(completion.awaiting_runtime(&controller));
        controller.config.theme = mj_core::config::UiTheme::Light;
        controller.config.bundles.insert(
            "created".into(),
            ProjectBundle {
                primary_repo: "created".into(),
                repositories: vec![],
            },
        );
        assert!(!completion.awaiting_runtime(&controller));
        assert_eq!(controller.config.theme, mj_core::config::UiTheme::Light);
        controller
            .config
            .bundles
            .get_mut("created")
            .unwrap()
            .primary_repo = "replaced".into();
        assert!(
            completion.awaiting_runtime(&controller),
            "the same id cannot authorize a different saved project"
        );
    }

    #[test]
    fn imported_session_followup_waits_for_both_runtime_record_and_bundle() {
        let session = lifecycle_session("imported", "workspace", SessionState::Stopped);
        let completion = DashboardIoUpdate::ImportedSessionApplied {
            result: Box::new(Ok(ImportedDashboardSessionApply {
                harness: "codex",
                native_session_id: "native".into(),
                session: session.clone(),
                bundle: ProjectBundle {
                    primary_repo: "project".into(),
                    repositories: vec![],
                },
            })),
        };
        let mut controller = Controller {
            config: Config::default(),
            state: State::default(),
        };
        assert!(completion.awaiting_runtime(&controller));
        let mut newer = session.clone();
        newer.session_title_override = Some("newer runtime title".into());
        controller.state.sessions.insert(newer.id.clone(), newer);
        assert!(completion.awaiting_runtime(&controller));
        controller.config.bundles.insert(
            session.bundle_id,
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![],
            },
        );
        assert!(!completion.awaiting_runtime(&controller));
        assert_eq!(
            controller.state.sessions["imported"].listed_title(),
            "newer runtime title"
        );
    }

    #[test]
    fn unconfirmed_registration_does_not_offer_to_replay_accepted_creation() {
        let completion = DashboardIoUpdate::CreateSession(Box::new(
            DashboardCreateSessionUpdate::Registered(Box::new(RegisteredDashboardSession {
                retry_launch: DashboardAction::None,
                session: lifecycle_session("created", "workspace", SessionState::Provisioning),
                remembered_container_size: None,
                cancelled: Arc::new(AtomicBool::new(false)),
            })),
        ));
        assert!(matches!(
            completion.runtime_wait_expired(),
            DashboardIoUpdate::RuntimeConfirmationMissing(_)
        ));
    }

    // Hard-won: fa50a0ac: launch verification found lifecycle notices identifying titled sessions by ID.
    #[test]
    fn notices_name_a_titled_session_by_its_title_and_others_by_short_id() {
        let mut titled = lifecycle_session("5e0fb24c-titled", "default", SessionState::Stopped);
        titled.session_title_override = Some("Fix the parser".into());
        let untitled = lifecycle_session("a1a8109b-untitled", "default", SessionState::Stopped);
        let state = State {
            sessions: [(titled.id.clone(), titled), (untitled.id.clone(), untitled)]
                .into_iter()
                .collect(),
            ..State::default()
        };
        assert_eq!(
            session_notice_name(&state, "5e0fb24c-titled"),
            "Fix the parser"
        );
        assert_eq!(session_notice_name(&state, "a1a8109b-untitled"), "a1a8109b");
        assert_eq!(session_notice_name(&state, "0badc0de-gone"), "0badc0de");
    }

    /// A session the dashboard created has only the title it was created
    /// with ("project via fake"), which the session list shows; "Launching"
    /// and "is ready" named it by id (launch finding R5-5).
    // Hard-won: b67f7811: lifecycle notices lost the creation title when the dashboard record reloaded.
    #[test]
    fn notices_name_an_unnamed_session_by_the_title_it_was_created_with() {
        let mut created = lifecycle_session("036b869b-created", "default", SessionState::Running);
        created.title = "project via fake".into();
        let state = State {
            sessions: [(created.id.clone(), created)].into_iter().collect(),
            ..State::default()
        };
        assert_eq!(
            session_notice_name(&state, "036b869b-created"),
            "project via fake"
        );
    }

    /// The daemon's runtime snapshot drops a destroyed session before the
    /// lifecycle reload lands, so the completion notice cannot find the
    /// record ("Permanently destroyed suspended session 5590965c" for
    /// "gamma", launch finding R5-4). It uses the name taken when the
    /// operation began; a record that is still there wins, because it has
    /// the newest name.
    // Hard-won: b67f7811: a destroyed record disappeared before its completion notice could name it.
    #[test]
    fn a_lifecycle_notice_keeps_the_name_taken_when_the_operation_began() {
        let empty = State::default();
        assert_eq!(
            lifecycle_notice_name(&empty, "5590965c-gamma", Some("gamma")),
            "gamma"
        );
        assert_eq!(
            lifecycle_notice_name(&empty, "5590965c-gamma", None),
            "5590965c"
        );
        let mut renamed = lifecycle_session("5590965c-gamma", "default", SessionState::Stopped);
        renamed.session_title_override = Some("gamma two".into());
        let state = State {
            sessions: [(renamed.id.clone(), renamed)].into_iter().collect(),
            ..State::default()
        };
        assert_eq!(
            lifecycle_notice_name(&state, "5590965c-gamma", Some("gamma")),
            "gamma two"
        );
    }

    // Hard-won: 8d16bf96: configured API-key references were treated as missing and blocked unrelated Settings saves.
    #[test]
    fn setup_save_resolves_api_key_references_and_rejects_missing_keys_before_writing() {
        use mj_core::config::{SecretResolver, with_secret_resolver};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let home = directory.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        // A key name no test runner exports: a profile that does not set its
        // provider's key inherits it from the process environment.
        std::fs::write(
            home.join("config.toml"),
            "model_provider = 'deepseek'\n[model_providers.deepseek]\nname = 'DeepSeek'\nbase_url = 'https://api.deepseek.com/v1'\nenv_key = 'MJ_TEST_SETUP_PROVIDER_KEY'\nwire_api = 'responses'\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("secrets.toml"),
            "MJ_TEST_SETUP_PROVIDER_KEY = 'test-secret'\n",
        )
        .unwrap();
        with_secret_resolver(SecretResolver::beside(&path), || {
            let mut value = serde_json::to_value(Config::default()).unwrap();
            value["profiles"] = serde_json::json!({"deepseek": {
                "kind": "codex", "home": home,
                "environment": {"MJ_TEST_SETUP_PROVIDER_KEY": {"from_secret": "MJ_TEST_SETUP_PROVIDER_KEY"}}
            }});
            let original: Config = serde_json::from_value(value.clone()).unwrap();
            original.save_to(&path).unwrap();
            let original_json = serde_json::to_string(&original).unwrap();
            value["notify"] = serde_json::json!({"bell": !original.notify.bell});
            let saved = save_setup_at(&path, &original_json, &value.to_string(), &State::default())
                .unwrap();
            assert_eq!(saved.notify.bell, !original.notify.bell);
            assert_eq!(
                saved.profiles["deepseek"].environment["MJ_TEST_SETUP_PROVIDER_KEY"],
                "test-secret"
            );
            let before = std::fs::read_to_string(&path).unwrap();
            assert!(!before.contains("test-secret"));
            assert!(before.contains("from_secret"));

            value["profiles"]["deepseek"]["environment"] = serde_json::json!({});
            let error = save_setup_at(&path, &original_json, &value.to_string(), &State::default())
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("authenticates with MJ_TEST_SETUP_PROVIDER_KEY"),
                "{error:#}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

            // A profile the edit leaves alone does not hold up the save, even
            // when it cannot start because its secret is gone.
            std::fs::write(directory.path().join("secrets.toml"), "").unwrap();
            let unready = Config::load_from(&path).unwrap();
            assert!(
                unready.profiles["deepseek"]
                    .ensure_ready("deepseek")
                    .is_err()
            );
            let unready_json = serde_json::to_string(&unready).unwrap();
            let mut value = serde_json::to_value(&unready).unwrap();
            value["notify"] = serde_json::json!({"bell": !unready.notify.bell});
            let saved =
                save_setup_at(&path, &unready_json, &value.to_string(), &State::default()).unwrap();
            assert_eq!(saved.notify.bell, !unready.notify.bell);
            assert!(
                std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("from_secret")
            );
        });
    }

    #[test]
    fn setup_save_refuses_to_remove_an_active_sessions_target_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut original = Config::default();
        original
            .targets
            .insert("podman".into(), mj_core::config::TargetTemplate::LocalBare);
        original.save_to(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let session = lifecycle_session("session-1", "default", SessionState::Running);
        let state = State {
            sessions: [(session.id.clone(), session)].into_iter().collect(),
            ..State::default()
        };
        let mut updated = original.clone();
        updated.targets.clear();
        let error = save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&updated).unwrap(),
            &state,
        )
        .unwrap_err();
        assert!(error.to_string().contains("running session"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // An unrelated preference remains editable even while a session needs repair.
        updated = original.clone();
        updated.advanced.detailed_activity_clocks = true;
        save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&updated).unwrap(),
            &state,
        )
        .unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap()
                .advanced
                .detailed_activity_clocks
        );
    }

    /// Launch finding R3-3: saving only the Jev checkbox wrote the built-in
    /// `[targets.docker]` and `[targets.podman]` blocks into config.toml. The
    /// dialog edits the effective config, which includes them; the file must
    /// keep only what the user set.
    // Hard-won: f15ebc51: saving unrelated Setup settings persisted built-in targets and made Doctor misreport Docker.
    #[test]
    fn setup_save_does_not_write_built_in_targets_the_user_never_configured() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save_to(&path).unwrap();
        let original = Config::default().with_local_targets();
        let mut edited = original.clone();
        edited.jev.enabled = false;
        let saved = save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&edited).unwrap(),
            &State::default(),
        )
        .unwrap();
        assert!(!saved.jev.enabled);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[jev]"), "{text}");
        assert!(!text.contains("[targets."), "{text}");
        assert!(Config::load_from(&path).unwrap().targets.is_empty());
    }

    /// Launch campaign finding A-3: choosing "Follows the terminal" in Settings
    /// removes `symbols` from `[advanced]` instead of leaving the old value.
    #[test]
    fn setup_save_removes_the_symbols_key_when_it_returns_to_unset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut original = Config::default();
        original.advanced.symbols = Some(mj_core::config::SymbolSet::Ascii);
        original.save_to(&path).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("symbols"));
        let mut edited = original.clone();
        edited.advanced.symbols = None;
        save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&edited).unwrap(),
            &State::default(),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("symbols"), "{text}");
        assert_eq!(Config::load_from(&path).unwrap().advanced.symbols, None);
    }

    #[test]
    fn settings_can_override_an_implicit_local_target_without_a_setup_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = Config::default().with_local_targets();
        let mut edited = original.clone();
        if let mj_core::config::TargetTemplate::LocalDocker { container } =
            edited.targets.get_mut("docker").unwrap()
        {
            container.image = "example.test/custom:latest".into();
        }
        let saved = save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&edited).unwrap(),
            &State::default(),
        )
        .unwrap();
        assert_eq!(saved.targets["docker"], edited.targets["docker"]);
        assert_eq!(
            Config::load_from(&path).unwrap().targets["docker"],
            edited.targets["docker"]
        );
    }

    #[test]
    fn setup_save_merges_unrelated_edits_and_refuses_conflicts_or_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = Config::default();
        original.save_to(&path).unwrap();
        let mut edited = original.clone();
        edited.sessions_side = mj_core::config::SessionsSide::Right;
        edited.theme = mj_core::config::UiTheme::Light;
        Config::update_to(&path, |current| {
            current.advanced.detailed_activity_clocks = true;
            Ok(())
        })
        .unwrap();
        let original_json = serde_json::to_string(&original).unwrap();
        let saved = save_setup_at(
            &path,
            &original_json,
            &serde_json::to_string(&edited).unwrap(),
            &State::default(),
        )
        .unwrap();
        assert_eq!(saved.sessions_side, mj_core::config::SessionsSide::Right);
        assert_eq!(saved.theme, mj_core::config::UiTheme::Light);
        assert!(saved.advanced.detailed_activity_clocks);
        assert_eq!(Config::load_from(&path).unwrap(), saved);

        let mut conflicting = original.clone();
        conflicting.phone.bind = "127.0.0.1:1234".parse().unwrap();
        Config::update_to(&path, |current| {
            current.phone.bind = "127.0.0.1:5678".parse().unwrap();
            Ok(())
        })
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            save_setup_at(
                &path,
                &original_json,
                &serde_json::to_string(&conflicting).unwrap(),
                &State::default(),
            )
            .unwrap_err()
            .to_string()
            .contains("another client")
        );
        let mut invalid = original.clone();
        invalid.review.profile = Some("missing".into());
        assert!(
            save_setup_at(
                &path,
                &original_json,
                &serde_json::to_string(&invalid).unwrap(),
                &State::default(),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn asynchronous_saves_leave_the_blocking_pool_available_for_connection_metadata() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (tracker, _) = CriticalOperationTracker::new();
            let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
            let (ready, mut started) = tokio::sync::mpsc::unbounded_channel();
            let release = Arc::new(tokio::sync::Notify::new());
            let mut tasks = Vec::new();
            for id in 0..64 {
                let ready = ready.clone();
                let release = release.clone();
                tasks.push(spawn_critical_async(
                    tracker.clone(),
                    format!("saving draft {id}"),
                    updates.clone(),
                    Duration::from_secs(2),
                    async move {
                        // The real connection needs a blocking metadata read.
                        tokio::task::spawn_blocking(|| ()).await?;
                        let released = release.notified();
                        tokio::pin!(released);
                        released.as_mut().enable();
                        ready.send(())?;
                        released.await;
                        Ok(())
                    },
                    move |result| DashboardIoUpdate::DetachedSessionState {
                        session_id: id.to_string(),
                        result,
                    },
                ));
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                for _ in 0..64 {
                    started.recv().await.unwrap();
                }
                // Chat preparation and other UI work can still use the pool.
                assert_eq!(
                    tokio::task::spawn_blocking(|| "chat ready").await.unwrap(),
                    "chat ready"
                );
            })
            .await
            .expect("network waits must not starve blocking work");
            release.notify_waiters();
            for task in tasks {
                task.await.unwrap();
            }
            for _ in 0..64 {
                assert!(matches!(
                    results.recv().await,
                    Some(DashboardIoUpdate::DetachedSessionState { result: Ok(()), .. })
                ));
            }
            assert!(
                tracker.blockers().is_empty(),
                "saves must release quit blockers"
            );
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    #[tokio::test]
    async fn unresponsive_save_reports_uncertain_durability_and_releases_quit() {
        let (tracker, _) = CriticalOperationTracker::new();
        let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
        let task = spawn_critical_async(
            tracker.clone(),
            "saving draft",
            updates,
            Duration::from_millis(20),
            std::future::pending::<Result<()>>(),
            |result| DashboardIoUpdate::DetachedSessionState {
                session_id: "muse".into(),
                result,
            },
        );
        assert_eq!(tracker.blockers().len(), 1);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        let Some(DashboardIoUpdate::DetachedSessionState {
            result: Err(error), ..
        }) = results.recv().await
        else {
            panic!("missing save failure");
        };
        assert!(error.contains("did not acknowledge"));
        assert!(error.contains("may still complete"));
        assert!(tracker.blockers().is_empty());
    }

    /// A job that dies must still answer, or the screen waits forever on work
    /// that is never coming back.
    // Hard-won: ceb1f1b0: a blocking job panic sent no completion and left the screen waiting forever.
    #[tokio::test]
    async fn a_panicking_blocking_job_still_reports_its_failure() {
        let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
        spawn_io(
            "write clipboard",
            updates,
            || -> Result<()> { panic!("boom") },
            DashboardIoUpdate::ClipboardWritten,
        )
        .await
        .unwrap();
        let Some(DashboardIoUpdate::ClipboardWritten(Err(error))) = results.recv().await else {
            panic!("a panicking job reported nothing");
        };
        assert!(error.contains("write clipboard"), "{error}");
        assert!(error.contains("boom"), "{error}");
    }

    // Hard-won: ceb1f1b0: a critical blocking job panic left both the operation and quit blocker unresolved.
    #[tokio::test]
    async fn a_panicking_critical_job_reports_its_failure_and_releases_quit() {
        let (tracker, _) = CriticalOperationTracker::new();
        let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
        spawn_critical_io(
            tracker.clone(),
            "writing the clipboard",
            updates,
            || -> Result<()> { panic!("boom") },
            DashboardIoUpdate::ClipboardWritten,
        )
        .await
        .unwrap();
        let Some(DashboardIoUpdate::ClipboardWritten(Err(error))) = results.recv().await else {
            panic!("a panicking critical job reported nothing");
        };
        assert!(error.contains("writing the clipboard"), "{error}");
        assert!(error.contains("boom"), "{error}");
        assert!(
            tracker.blockers().is_empty(),
            "a panicking job must not block quit"
        );
    }

    // Hard-won: ceb1f1b0: a cancellable blocking job panic left no failure update and retained quit admission.
    #[tokio::test]
    async fn a_panicking_cancellable_job_reports_its_failure_and_releases_quit() {
        let (tracker, _) = CriticalOperationTracker::new();
        let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
        spawn_cancellable_io(
            tracker.clone(),
            "writing the clipboard",
            updates,
            |_cancelled| -> Result<()> { panic!("boom") },
            DashboardIoUpdate::ClipboardWritten,
        )
        .await
        .unwrap();
        let Some(DashboardIoUpdate::ClipboardWritten(Err(error))) = results.recv().await else {
            panic!("a panicking cancellable job reported nothing");
        };
        assert!(error.contains("writing the clipboard"), "{error}");
        assert!(error.contains("boom"), "{error}");
        assert!(
            tracker.blockers().is_empty(),
            "a panicking job must not block quit"
        );
    }

    #[test]
    fn quick_github_bundle_uses_collision_suffix_and_reuses_matching_source() {
        let mut config = Config::default();
        config.bundles.insert(
            "app".into(),
            ProjectBundle {
                primary_repo: "app".into(),
                repositories: vec![ProjectRepository {
                    id: "app".into(),
                    github: Some("other/app".into()),
                    local: None,
                    destination: "app".into(),
                    git_ref: None,
                }],
            },
        );

        let created =
            create_quick_bundle(&mut config, "https://github.com/example/app.git").unwrap();
        assert_eq!(created, "app-2");
        assert_eq!(
            create_quick_bundle(&mut config, "example/app").unwrap(),
            "app-2"
        );
        assert_eq!(config.bundles.len(), 2);
    }

    const LIFECYCLE_RELOAD_CHILD: &str = "MJ_TEST_LIFECYCLE_RELOAD_CHILD";

    /// A load started before a runtime update must not resurrect old records
    /// or remove a newer session when the lifecycle completion arrives.
    #[tokio::test]
    async fn a_lifecycle_reload_preserves_newer_runtime_records() {
        if crate::test_support::rerun_in_isolated_child(
            LIFECYCLE_RELOAD_CHILD,
            "dashboard::io::tests::a_lifecycle_reload_preserves_newer_runtime_records",
        ) {
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();

        let mut config = Config::default();
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                kind: HarnessKind::Codex,
                home: PathBuf::from("/home/dev/.codex"),
                environment: Default::default(),
                context_window_bytes: None,
                subagents: Default::default(),
                guardian_review_model: None,
            },
        );
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: Some("owner/project".into()),
                    local: None,
                    destination: PathBuf::from("project"),
                    git_ref: None,
                }],
            },
        );
        config.targets.insert(
            "podman".into(),
            mj_core::config::TargetTemplate::LocalPodman {
                container: mj_core::config::ContainerTemplate {
                    build_cache: None,
                    image: "example.invalid/hel-test:latest".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: Default::default(),
                    workspace_storage: Default::default(),
                },
            },
        );
        config.save().unwrap();

        let database = mj_controller::database::database_path();
        let alpha = mj_controller::database::create_workspace_at(&database, "alpha").unwrap();
        let beta = mj_controller::database::create_workspace_at(&database, "beta").unwrap();
        mj_controller::database::save_session(&lifecycle_session(
            "session-alpha-live",
            &alpha.id,
            SessionState::Running,
        ))
        .unwrap();
        mj_controller::database::save_session(&lifecycle_session(
            "session-alpha-stopped",
            &alpha.id,
            SessionState::Stopped,
        ))
        .unwrap();
        mj_controller::database::save_session(&lifecycle_session(
            "session-beta-live",
            &beta.id,
            SessionState::Running,
        ))
        .unwrap();

        let (updates_tx, mut updates_rx) =
            tokio::sync::mpsc::unbounded_channel::<DashboardIoUpdate>();
        spawn_lifecycle_reload(
            LifecycleReload {
                update: DashboardLifecycleUpdate {
                    session_id: "session-beta-live".into(),
                    result: Ok(LifecycleSuccess::Created),
                    operation: Arc::new(AtomicBool::new(false)),
                },
                operation: None,
            },
            beta.id.clone(),
            "client-1".into(),
            updates_tx,
        );

        let DashboardIoUpdate::LifecycleReloaded(reloaded) =
            updates_rx.recv().await.expect("the reload reports back")
        else {
            panic!("the reload reports through LifecycleReloaded");
        };
        let loaded = reloaded.result.expect("the reload succeeds");
        let mut current = State::default();
        current.sessions.insert(
            "newer-session".into(),
            lifecycle_session("newer-session", &beta.id, SessionState::Running),
        );
        loaded.apply(&mut current);
        assert_eq!(
            current
                .sessions
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["newer-session"],
            "a completed background load cannot replace newer runtime records"
        );
    }

    fn lifecycle_session(id: &str, workspace_id: &str, state: SessionState) -> SessionRecord {
        SessionRecord {
            project: None,
            target_runtime: None,
            launch_base: None,
            launch_branch: None,
            checkout: None,
            publication: None,
            build_cache: None,
            container_workspace: None,
            subagents: None,
            create_managed_worktree: None,
            workspace_id: workspace_id.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: id.into(),
            title: id.into(),
            harness_kind: HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            review: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-09-04T00:00:00Z".into(),
            updated_at: "2026-09-04T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }
}
