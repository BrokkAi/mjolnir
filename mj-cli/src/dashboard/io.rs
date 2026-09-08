//! One-shot background jobs the dashboard starts and the updates they report.
//!
//! Each job runs on a blocking task and answers over the dashboard's single
//! [`DashboardIoUpdate`] channel, so no filesystem, database, or process work
//! ever happens on the render loop. Failures travel as `Err` payloads rather
//! than being dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hel::hel_config::{HarnessKind, HelConfig, ProjectBundle};
use hel::hel_database::DetachedSessionDraft;
use hel::hel_state::{
    HelState, MaterializedSession, MovePreparation, ProjectSourceIdentity, SessionRecord,
    SessionState,
};
use hel::hel_targets::CancellableProcessExecutor;
use hel_tui::{
    DashboardAction, PreparedMaterializedSessionDetail, PreparedMaterializedSessionSummary,
    ReviewSettingsChoices, ReviewSettingsDiscoveryResult, SessionOperationKind, WebViewerAccess,
};
use hel_tui::{WorkspaceDraftEntry, WorkspaceManagementEntry};
use mj_controller::hel_controller::Controller;
use mj_controller::hel_controller::ResumeRepositorySourcePreflight;
use mj_controller::hel_session_manager::SessionManagerControl;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::daemon;
use crate::dashboard::{CriticalOperationTracker, DashboardContext};
use crate::import::{DashboardImportSuccess, PendingDashboardImport, persist_imported_session};
use crate::pollers::{
    LifecycleSuccess, LifecycleUpdate, WorkerRecordPersistence, WorkerRecordPersistenceOutcome,
};
use crate::short_id;

/// Everything the dashboard learns from a background job.
pub(crate) enum DashboardIoUpdate {
    /// A workspace manager snapshot loaded away from the event loop. The
    /// selection hint is set only by a successful create; the dashboard must
    /// still verify the modal generation before applying it.
    WorkspaceManagement {
        generation: u64,
        result: std::result::Result<WorkspaceManagementResult, String>,
    },
    WorkspacePaneSizes {
        result: std::result::Result<BTreeMap<String, hel::hel_workspace::PaneSizes>, String>,
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
    ProjectSource {
        session_id: String,
        result: std::result::Result<ProjectSourceIdentity, String>,
    },
    ChatOpened {
        generation: u64,
        session_id: String,
        result: Box<std::result::Result<mj_chat::hel_chat::PreparedChat, String>>,
    },
    /// The daemon refused a review action. The message is a sentence for the
    /// person who pressed the key, so it goes back to the chat that sent it.
    ReviewRefused {
        session_id: String,
        message: String,
    },
    CreateSession(Box<DashboardCreateSessionUpdate>),
    StartupConfig(HelConfig),
    RenameSession {
        session_id: String,
        title: String,
        result: std::result::Result<String, String>,
    },
    ContainerSettings {
        session_id: String,
        result: std::result::Result<Controller, String>,
    },
    TargetTest {
        target_id: String,
        result: std::result::Result<(), String>,
    },
    ConfigRename {
        what: String,
        result: std::result::Result<Controller, String>,
    },
    ConfigReloaded(std::result::Result<Controller, String>),
    WebAccess {
        generation: u64,
        access: WebViewerAccess,
    },
    WebListeners {
        generation: u64,
        result: Result<Vec<hel_tui::WebListenerProcess>, String>,
    },
    WebAccessError {
        generation: u64,
        error: String,
    },
    SetupSaved {
        generation: u64,
        result: std::result::Result<HelConfig, String>,
    },
    SetupDiscovered {
        generation: u64,
        result: std::result::Result<HelConfig, String>,
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
        choices: mj_controller::hel_review_settings::ReviewCapabilityChoices,
    },
    ReviewSettingsSaved {
        result: std::result::Result<HelConfig, String>,
    },
    SpinnerStyleSaved {
        result: std::result::Result<HelConfig, String>,
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
        sizes: BTreeMap<String, Option<u64>>,
    },
    WorkerDiagnosis {
        session_id: String,
        episode_id: u64,
        result: std::result::Result<Option<String>, String>,
    },
    MountCompletions {
        prefix: String,
        result: std::result::Result<Vec<String>, String>,
    },
    MountValidation {
        source: String,
        result: std::result::Result<Option<String>, String>,
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
    ProjectValidation {
        directory: String,
        result: std::result::Result<(), String>,
    },
    /// Clipboard providers may use a blocking desktop IPC call. The result
    /// is delivered here after that work finishes on a blocking task.
    ClipboardText(std::result::Result<String, String>),
    /// A copied selection reached the desktop clipboard, or did not. Only a
    /// failure needs reporting: the notice for a successful copy is already
    /// on screen.
    ClipboardWritten(std::result::Result<(), String>),
    /// The set of native sessions the resume dialog hides, read from Hel's
    /// database.
    HiddenNativeSessions {
        result: std::result::Result<BTreeSet<(HarnessKind, String)>, String>,
    },
    /// A hide or reveal that has already been applied optimistically. Only a
    /// failure needs handling; `target` says what to put back so no row can
    /// stay out of step with what is stored.
    ArchiveWrite {
        what: String,
        target: ArchiveWriteTarget,
        result: std::result::Result<(), String>,
    },
}

/// Which hidden-row store an archive write was aimed at, and what the record
/// held before the optimistic update overwrote it.
pub(crate) enum ArchiveWriteTarget {
    /// A Hel session record. Its archived flag lives in two in-memory copies —
    /// the controller's state and the dashboard's — and both are restored.
    Session { session_id: String, archived: bool },
    /// The hidden native session set, which is re-read from the database
    /// rather than reconstructed.
    HiddenNativeSessions,
}

/// Puts back what an archive write did not manage to store. Returns whether
/// the hidden native set still has to be re-read from the database.
pub(crate) fn revert_archive_write(
    target: &ArchiveWriteTarget,
    state: &mut HelState,
    dashboard: &mut hel_tui::DashboardState,
) -> bool {
    match target {
        ArchiveWriteTarget::Session {
            session_id,
            archived,
        } => {
            if let Some(session) = state.sessions.get_mut(session_id) {
                session.archived = *archived;
            }
            dashboard.set_session_archived(session_id, *archived);
            false
        }
        ArchiveWriteTarget::HiddenNativeSessions => true,
    }
}

pub(crate) struct ActiveLifecycleOperation {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) kind: SessionOperationKind,
}

pub(crate) struct WorkspaceManagementResult {
    revision: u64,
    pub(crate) entries: Vec<WorkspaceManagementEntry>,
    pub(crate) select_workspace: Option<String>,
    pub(crate) deleted_workspace_id: Option<String>,
}

pub(crate) struct RegisteredDashboardSession {
    generation: Option<u64>,
    session: SessionRecord,
    remembered_container_size: Option<(String, hel::hel_state::HostContainerSize)>,
    cancelled: Arc<AtomicBool>,
}

pub(crate) enum DashboardCreateSessionUpdate {
    DirtyLocal {
        action: DashboardAction,
        repositories: Vec<String>,
    },
    Registered(Box<RegisteredDashboardSession>),
    Failed {
        generation: Option<u64>,
        error: String,
    },
}

pub(crate) struct ImportedDashboardSessionApply {
    harness: &'static str,
    native_session_id: String,
    session: SessionRecord,
    bundle_id: String,
    bundle: ProjectBundle,
}

pub(crate) struct CreatedBundleUpdate {
    config: HelConfig,
    bundle_id: String,
}

pub(crate) struct ResumeRepositoryPreflightApply {
    pub(crate) config: Option<HelConfig>,
    pub(crate) preflight: ResumeRepositorySourcePreflight,
}

pub(crate) struct LifecycleReload {
    pub(crate) update: LifecycleUpdate,
    pub(crate) operation: Option<ActiveLifecycleOperation>,
}

pub(crate) struct LifecycleReloaded {
    reload: LifecycleReload,
    result: std::result::Result<Controller, String>,
}

/// Runs one blocking job off the loop and reports its outcome on the
/// dashboard's I/O channel. Errors are formatted once, here, so no caller can
/// quietly drop one.
pub(crate) fn spawn_io<T>(
    operation: &'static str,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce() -> Result<T> + Send + 'static,
    report: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let result = work().map_err(|error| {
            let error = format!("{error:#}");
            tracing::warn!(operation, %error, "dashboard background operation failed");
            error
        });
        if let Err(error) = updates.send(report(result)) {
            tracing::debug!(operation, %error, "dashboard background result dropped after shutdown");
        }
    })
}

/// Runs a user-authored mutation off the event loop and keeps dashboard exit
/// pending until the mutation has reached its durable boundary.
pub(crate) fn spawn_critical_io<T>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce() -> Result<T> + Send + 'static,
    report: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    let label = label.into();
    let guard = tracker.begin(label.clone());
    tokio::task::spawn_blocking(move || {
        let result = work().map_err(|error| {
            let error = format!("{error:#}");
            tracing::warn!(operation = %label, %error, "critical dashboard operation failed");
            error
        });
        if let Err(error) = updates.send(report(result)) {
            tracing::debug!(operation = %label, %error, "critical dashboard result dropped after shutdown");
        }
        drop(guard);
    })
}

/// Network waits must not hold a blocking-pool thread: connecting to the
/// daemon itself needs a blocking metadata read. Bound acknowledgement waits
/// so a silent daemon cannot indefinitely prevent dashboard exit.
fn spawn_async_job<T: Send + 'static>(
    tracker: Option<CriticalOperationTracker>,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
    report: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()> {
    let label = label.into();
    let guard = tracker.map(|tracker| tracker.begin(label.clone()));
    tokio::spawn(async move {
        let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(work));
        let result = match tokio::time::timeout(timeout, task).await {
            Ok(Ok(result)) => result.map_err(|error| format!("{error:#}")),
            Ok(Err(error)) => Err(format!("{label} task failed: {error}")),
            Err(_) => Err(format!(
                "{label}: the daemon did not acknowledge the save within {} seconds; it may still complete. Reconnect to verify the saved state",
                timeout.as_secs()
            )),
        };
        if let Err(error) = &result {
            tracing::error!(operation = %label, %error, "dashboard save was not confirmed");
        }
        if let Err(error) = updates.send(report(result)) {
            tracing::debug!(operation = %label, %error, "dashboard save result dropped after shutdown");
        }
        drop(guard);
    })
}

fn spawn_critical_async<T: Send + 'static>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
    report: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()> {
    spawn_async_job(Some(tracker), label, updates, timeout, work, report)
}

fn spawn_background_async<T: Send + 'static>(
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
    report: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()> {
    spawn_async_job(None, label, updates, timeout, work, report)
}

/// Loads the manager's complete view from the daemon. Workspace listings and
/// their detached-draft previews are read through the same client so the
/// snapshot is ordered and every failure reaches the dashboard.
async fn load_workspace_management_entries(
    daemon: &mut daemon::DaemonClient,
) -> Result<Vec<WorkspaceManagementEntry>> {
    let listings = daemon.list_workspaces().await?;
    let mut entries = Vec::with_capacity(listings.len());
    for listing in listings {
        let snapshot = daemon.snapshot(listing.workspace.id.clone()).await?;
        let drafts = snapshot
            .drafts
            .into_iter()
            .map(|draft| WorkspaceDraftEntry {
                id: draft.id,
                session_id: draft.session_id,
                source: draft.source,
                saved_at: draft.saved_at,
                owner_pid: draft.owner_pid,
            })
            .collect();
        entries.push(WorkspaceManagementEntry {
            workspace: snapshot.workspace,
            drafts,
        });
    }
    Ok(entries)
}

fn spawn_workspace_management_operation(
    generation: u64,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: Option<CriticalOperationTracker>,
    work: impl std::future::Future<Output = Result<WorkspaceManagementResult>> + Send + 'static,
) -> JoinHandle<()> {
    let report = move |result| DashboardIoUpdate::WorkspaceManagement { generation, result };
    match tracker {
        Some(tracker) => {
            spawn_critical_async(tracker, label, updates, SAVE_ACK_TIMEOUT, work, report)
        }
        None => spawn_background_async(label, updates, SAVE_ACK_TIMEOUT, work, report),
    }
}

pub(crate) fn spawn_workspace_management_load(
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(generation, "loading workspaces", updates, None, async {
        let mut daemon = daemon::connect_or_start().await?;
        let revision = daemon
            .runtime_snapshot(String::new(), 0, true)
            .await?
            .revision;
        Ok(WorkspaceManagementResult {
            revision,
            entries: load_workspace_management_entries(&mut daemon).await?,
            select_workspace: None,
            deleted_workspace_id: None,
        })
    })
}

/// Reads layouts for workspace ids discovered after the dashboard opened.
/// Loading is deliberately separate from the runtime snapshot so a late
/// result cannot overwrite a pane size the local client already edited.
pub(crate) fn spawn_workspace_pane_sizes_load(
    workspace_ids: Vec<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_io(
        "load workspace pane sizes",
        updates,
        move || {
            workspace_ids
                .into_iter()
                .map(|workspace_id| {
                    let sizes = hel::hel_database::load_workspace_pane_sizes(&workspace_id)?;
                    Ok((workspace_id, sizes))
                })
                .collect()
        },
        |result| DashboardIoUpdate::WorkspacePaneSizes { result },
    )
}

pub(crate) fn spawn_workspace_create(
    generation: u64,
    name: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(
        generation,
        "creating workspace",
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            let workspace = daemon.create_workspace(name).await?;
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: Some(workspace.id),
                deleted_workspace_id: None,
            })
        },
    )
}

pub(crate) fn spawn_workspace_rename(
    generation: u64,
    workspace_id: String,
    name: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(
        generation,
        "renaming workspace",
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            daemon.rename_workspace(workspace_id, name).await?;
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: None,
                deleted_workspace_id: None,
            })
        },
    )
}

pub(crate) fn spawn_workspace_delete(
    generation: u64,
    workspace_id: String,
    force: bool,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let deleted_workspace_id = workspace_id.clone();
    spawn_workspace_management_operation(
        generation,
        if force {
            "force deleting workspace"
        } else {
            "deleting workspace"
        },
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            if force {
                daemon.force_delete_workspace(workspace_id).await?;
            } else {
                daemon.delete_workspace(workspace_id).await?;
            }
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: None,
                deleted_workspace_id: Some(deleted_workspace_id),
            })
        },
    )
}

pub(crate) fn spawn_workspace_draft_recovery(
    generation: u64,
    draft_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(
        generation,
        "recovering workspace draft",
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            daemon.recover_draft(draft_id).await?;
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: None,
                deleted_workspace_id: None,
            })
        },
    )
}

const SAVE_ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// Like [`spawn_critical_io`], with a cooperative cancellation flag for work
/// that can own a subprocess while the dashboard is shutting down.
pub(crate) fn spawn_cancellable_io<T>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
    report: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    spawn_cancellable_io_with_token(tracker, label, updates, work, report).1
}

pub(crate) fn spawn_cancellable_io_with_token<T>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
    report: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> (Arc<AtomicBool>, JoinHandle<()>)
where
    T: Send + 'static,
{
    let label = label.into();
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable(label.clone(), cancelled.clone());
    let worker_cancelled = cancelled.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let result = work(worker_cancelled).map_err(|error| {
            let error = format!("{error:#}");
            tracing::warn!(operation = %label, %error, "cancellable dashboard operation failed");
            error
        });
        if let Err(error) = updates.send(report(result)) {
            tracing::debug!(operation = %label, %error, "cancellable dashboard result dropped after shutdown");
        }
        drop(guard);
    });
    (cancelled, worker)
}

/// Reads the hidden-session set out of Hel's own database. Called when the
/// resume dialog opens and again whenever a hide or reveal fails to commit.
pub(crate) fn spawn_hidden_native_sessions_load(
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_io(
        "load hidden native sessions",
        updates,
        hel::hel_database::hidden_native_sessions,
        |result| DashboardIoUpdate::HiddenNativeSessions { result },
    )
}

/// Discovers advertised reviewer choices from one connected worker in
/// a supervised asynchronous task. A fresh generation is included in the
/// reply; the TUI drops replies for edits that happened after this request.
pub(crate) fn spawn_review_settings_discovery(
    control: SessionManagerControl,
    request: mj_controller::hel_review_settings::ReviewDiscoveryRequest,
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> Arc<AtomicBool> {
    let profile_id = request.profile.clone();
    let model = request.model.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable("loading review choices", cancelled.clone());
    let worker_cancelled = cancelled.clone();
    tokio::spawn(async move {
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let discovery = mj_controller::hel_review_settings::discover_review_settings(
            control,
            request,
            worker_cancelled,
            progress_tx,
        );
        tokio::pin!(discovery);
        let result = loop {
            tokio::select! {
                // Drain ready choices before final completion so a queued progress
                // event can never arrive after the final discovery result.
                biased;
                Some(choices) = progress_rx.recv() => {
                    if let Err(error) = updates.send(DashboardIoUpdate::ReviewSettingsChoices {
                        generation,
                        profile_id: profile_id.clone(),
                        model: model.clone(),
                        choices,
                    }) {
                        tracing::debug!(%error, "review choices dropped after dashboard shutdown");
                    }
                }
                result = &mut discovery => break result,
            }
        }
        .map(|outcome| match outcome {
            mj_controller::hel_review_settings::ReviewDiscoveryOutcome::Available {
                choices,
                cleanup_warning,
            } => ReviewSettingsDiscoveryResult::Available {
                choices: review_settings_choices(choices),
                cleanup_warning,
            },
            mj_controller::hel_review_settings::ReviewDiscoveryOutcome::Unavailable => {
                ReviewSettingsDiscoveryResult::Unavailable
            }
        })
        .map_err(|error| format!("{error:#}"));
        if let Err(error) = updates.send(DashboardIoUpdate::ReviewSettingsDiscovered {
            generation,
            profile_id,
            model,
            result,
        }) {
            tracing::debug!(%error, "review settings discovery result dropped after dashboard shutdown");
        }
        drop(guard);
    });
    cancelled
}

fn review_settings_choices(
    choices: mj_controller::hel_review_settings::ReviewCapabilityChoices,
) -> ReviewSettingsChoices {
    ReviewSettingsChoices {
        model_choices: choices.model_choices,
        effort_choices: choices.effort_choices,
        effort_capabilities_discovered: choices.effort_capabilities_discovered,
    }
}

pub(crate) fn spawn_setup_discovery(
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_cancellable_io(
        tracker,
        "detecting setup",
        updates,
        move |cancelled| {
            use mj_controller::hel_setup::{self, DEFAULT_IMAGE, RuntimeKind};
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(30));
            let discovery = hel_setup::discover_current(&executor);
            let mut config = hel_setup::build_config(
                &discovery.homes,
                discovery.repository.as_ref(),
                RuntimeKind::Podman,
                DEFAULT_IMAGE,
            );
            config.targets.clear();
            for runtime in discovery.runtimes.iter().filter(|runtime| runtime.usable) {
                let (id, target) = hel_setup::local_runtime_target(runtime.kind, DEFAULT_IMAGE);
                config.targets.insert(id.to_owned(), target);
            }
            #[cfg(unix)]
            config.targets.insert(
                "localhost".into(),
                hel::hel_config::TargetTemplate::LocalBare,
            );
            Ok(config)
        },
        move |result| DashboardIoUpdate::SetupDiscovered { generation, result },
    )
}

pub(crate) fn spawn_setup_save(
    generation: u64,
    original: String,
    updated: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_critical_io(
        tracker,
        "saving setup",
        updates,
        move || save_setup_at(&hel::hel_config::config_path(), &original, &updated),
        move |result| DashboardIoUpdate::SetupSaved { generation, result },
    )
}

fn save_setup_at(path: &std::path::Path, original: &str, updated: &str) -> Result<HelConfig> {
    let original: serde_json::Value = serde_json::from_str(original)?;
    let updated_config: HelConfig = serde_json::from_str(updated)?;
    updated_config.validate()?;
    let updated = serde_json::to_value(updated_config)?;
    HelConfig::update_to(path, |config| {
        let current = serde_json::to_value(&*config)?;
        let merged = merge_setup_edit(Some(&original), Some(&updated), Some(&current), "Setup")?
            .context("setup cannot remove the configuration")?;
        *config = serde_json::from_value(merged)?;
        Ok(())
    })
    .map(|(config, ())| config)
}

/// Apply only fields the dialog changed; refuse conflicting concurrent edits.
fn merge_setup_edit(
    original: Option<&serde_json::Value>,
    updated: Option<&serde_json::Value>,
    current: Option<&serde_json::Value>,
    path: &str,
) -> Result<Option<serde_json::Value>> {
    use serde_json::Value;
    if original == updated || updated == current {
        return Ok(current.cloned());
    }
    if original == current {
        return Ok(updated.cloned());
    }
    if updated.is_some_and(Value::is_object)
        && original.is_none_or(Value::is_object)
        && current.is_some_and(Value::is_object)
    {
        let keys = original
            .into_iter()
            .chain(updated)
            .chain(current)
            .flat_map(|value| value.as_object().unwrap().keys())
            .collect::<BTreeSet<_>>();
        let mut merged = serde_json::Map::new();
        for key in keys {
            if let Some(value) = merge_setup_edit(
                original.and_then(|v| v.get(key)),
                updated.and_then(|v| v.get(key)),
                current.and_then(|v| v.get(key)),
                &format!("{path} / {key}"),
            )? {
                merged.insert(key.clone(), value);
            }
        }
        return Ok(Some(Value::Object(merged)));
    }
    bail!("{path} changed in another client. Reopen Setup to edit the latest value.")
}

pub(crate) fn spawn_spinner_style_save(
    style: hel::hel_config::SpinnerStyle,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_critical_io(
        tracker,
        "saving spinner style",
        updates,
        move || {
            HelConfig::update(|config| {
                config.spinner = style;
                Ok(())
            })
            .map(|(config, ())| config)
        },
        |result| DashboardIoUpdate::SpinnerStyleSaved { result },
    )
}

pub(crate) fn spawn_review_settings_save(
    review: hel::hel_config::ReviewConfig,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_critical_io(
        tracker,
        "saving review settings",
        updates,
        move || HelConfig::save_review(review),
        |result| DashboardIoUpdate::ReviewSettingsSaved { result },
    )
}

/// Resolves one raw checkout's Git origin off the event loop. Each session is
/// independent, so callers can launch these concurrently and redraw as the
/// answers arrive.
pub(crate) fn spawn_project_source_resolution(
    controller: &Controller,
    session_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let config = controller.config.clone();
    let session = controller.state.sessions.get(&session_id).cloned();
    let source_controller = Controller {
        config,
        state: HelState {
            sessions: session
                .map(|session| [(session_id.clone(), session)].into_iter().collect())
                .unwrap_or_default(),
            ..HelState::default()
        },
    };
    let reported_session_id = session_id.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable(
        format!("resolving project for {}", short_id(&session_id)),
        cancelled.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let executor =
            CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(8));
        let result = source_controller
            .resolve_session_project_source(&session_id, &executor)
            .map_err(|error| format!("{error:#}"));
        if let Err(error) = updates.send(DashboardIoUpdate::ProjectSource {
            session_id: reported_session_id,
            result,
        }) {
            tracing::debug!(%error, "project source result dropped after dashboard shutdown");
        }
        drop(guard);
    })
}

/// Persists one archive or unarchive. The dashboard already moved the row, so
/// only the failure path matters here: `target` carries what to restore.
pub(crate) fn spawn_archive_write(
    what: String,
    target: ArchiveWriteTarget,
    write: impl FnOnce() -> Result<()> + Send + 'static,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_critical_io(tracker, what.clone(), updates, write, move |result| {
        DashboardIoUpdate::ArchiveWrite {
            what,
            target,
            result,
        }
    })
}

/// A controller that answers target questions from configuration alone, for
/// the completions and validations the launch dialog asks for.
pub(crate) fn config_only_controller(config: HelConfig) -> Controller {
    Controller {
        config,
        state: HelState::default(),
    }
}

/// What every session lifecycle operation needs to run off the loop.
pub(crate) struct LifecycleOperationRequest {
    pub(crate) session_id: String,
    pub(crate) kind: SessionOperationKind,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) updates: UnboundedSender<LifecycleUpdate>,
}

/// Runs one session lifecycle operation on a blocking task.
///
/// Every one of them reloads the controller so it acts on durable state, then
/// answers on the lifecycle channel whatever happens. The daemon owns
/// lifecycle/recovery serialization.
pub(crate) fn spawn_lifecycle_operation(
    request: LifecycleOperationRequest,
    tracker: CriticalOperationTracker,
    work: impl FnOnce(&mut Controller, Arc<AtomicBool>) -> Result<LifecycleSuccess> + Send + 'static,
) {
    let LifecycleOperationRequest {
        session_id,
        kind,
        cancelled,
        updates,
    } = request;
    let guard = tracker.begin_cancellable(
        format!(
            "{} session {}",
            kind.label().to_ascii_lowercase(),
            short_id(&session_id)
        ),
        cancelled.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<LifecycleSuccess> {
            let mut controller = Controller::load()?;
            work(&mut controller, cancelled)
        })()
        .map_err(|error| format!("{error:#}"));
        if let Err(error) = updates.send(LifecycleUpdate {
            session_id,
            result,
            deferred_cleanup: false,
        }) {
            tracing::debug!(%error, "lifecycle result dropped after dashboard shutdown");
        }
        drop(guard);
    });
}

pub(crate) fn spawn_materialized_session_projection(
    materialized: MaterializedSession,
    viewed_through_event_ordinal: u64,
    previous: hel_tui::MaterializedProjectionCache,
    updates: UnboundedSender<DashboardIoUpdate>,
    permits: Arc<tokio::sync::Semaphore>,
) {
    let session_id = materialized.session_id.clone();
    tokio::spawn(async move {
        let result = match permits.acquire_owned().await {
            Ok(permit) => {
                let result = tokio::task::spawn_blocking(move || {
                    PreparedMaterializedSessionDetail::from_materialized(
                        materialized,
                        viewed_through_event_ordinal,
                        previous,
                    )
                })
                .await
                .map(Box::new)
                .map_err(|error| format!("session projection task failed: {error}"));
                drop(permit);
                result
            }
            Err(error) => Err(format!("session projection worker stopped: {error}")),
        };
        if let Err(error) =
            updates.send(DashboardIoUpdate::MaterializedSessionProjection { session_id, result })
        {
            tracing::debug!(%error, "session projection result dropped after dashboard shutdown");
        }
    });
}

pub(crate) fn spawn_stored_session_summary(
    session_id: String,
    viewed_through_event_ordinal: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    let reported_session_id = session_id.clone();
    spawn_io(
        "load stored session summary",
        updates,
        move || {
            let summary = hel::hel_database::load_materialized_session_summary(&session_id)?
                .with_context(|| format!("session {session_id} has no stored projection"))?;
            Ok(PreparedMaterializedSessionSummary::from_materialized(
                summary,
                viewed_through_event_ordinal,
            ))
        },
        move |result| DashboardIoUpdate::StoredSessionSummary {
            session_id: reported_session_id,
            result,
        },
    );
}

pub(crate) fn spawn_lifecycle_reload(
    reload: LifecycleReload,
    workspace_id: String,
    client_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    spawn_io(
        "reload lifecycle state",
        updates,
        move || {
            let mut controller = Controller::load()?;
            super::retain_workspace_sessions(&mut controller, &workspace_id, &client_id)?;
            Ok(controller)
        },
        move |result| {
            DashboardIoUpdate::LifecycleReloaded(Box::new(LifecycleReloaded { reload, result }))
        },
    );
}

pub(crate) fn spawn_dashboard_rename(
    session_id: String,
    title: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let renamed_session_id = session_id.clone();
    let requested_title = title.clone();
    spawn_critical_async(
        tracker,
        format!("renaming session {}", short_id(&session_id)),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .set_session_title(renamed_session_id, requested_title)
                .await
        },
        move |result| DashboardIoUpdate::RenameSession {
            session_id,
            title,
            result,
        },
    );
}

pub(crate) struct ConfigRenameRequest {
    pub(crate) what: String,
    pub(crate) old_id: String,
    pub(crate) new_id: String,
    pub(crate) profile: bool,
    pub(crate) workspace_id: String,
    pub(crate) client_id: String,
}

pub(crate) fn spawn_config_rename(
    request: ConfigRenameRequest,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let ConfigRenameRequest {
        what,
        old_id,
        new_id,
        profile,
        workspace_id,
        client_id,
    } = request;
    let guard = tracker.begin(format!("renaming {what}"));
    tokio::spawn(async move {
        let result = async {
            let mut daemon = daemon::connect_existing().await?;
            if profile {
                daemon.rename_profile(old_id, new_id).await?;
            } else {
                daemon.rename_target(old_id, new_id).await?;
            }
            tokio::task::spawn_blocking(move || {
                let mut controller = Controller::load()?;
                super::retain_workspace_sessions(&mut controller, &workspace_id, &client_id)?;
                Ok::<_, anyhow::Error>(controller)
            })
            .await
            .context("configuration reload task panicked")?
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        drop(guard);
        if let Err(error) = updates.send(DashboardIoUpdate::ConfigRename { what, result }) {
            tracing::debug!(%error, "config rename result dropped after dashboard shutdown");
        }
    });
}

/// What the container editor asks the controller to persist.
pub(crate) struct ContainerSettingsRequest {
    pub(crate) session_id: String,
    pub(crate) cpus: Option<String>,
    pub(crate) memory: Option<String>,
    pub(crate) additional_mounts: Vec<hel::hel_targets::AdditionalMount>,
    pub(crate) mount_history: Vec<std::path::PathBuf>,
    pub(crate) workspace_id: String,
    pub(crate) client_id: String,
}

pub(crate) fn spawn_dashboard_container_settings(
    request: ContainerSettingsRequest,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let session_id = request.session_id.clone();
    let runtime = tokio::runtime::Handle::current();
    spawn_critical_io(
        tracker,
        format!("saving container settings for {}", short_id(&session_id)),
        updates,
        move || {
            let ContainerSettingsRequest {
                session_id,
                cpus,
                memory,
                additional_mounts,
                mount_history,
                workspace_id,
                client_id,
            } = request;
            runtime.block_on(async {
                daemon::connect_or_start()
                    .await?
                    .set_session_container_settings(
                        session_id,
                        cpus,
                        memory,
                        additional_mounts,
                        mount_history,
                    )
                    .await
            })?;
            // Return a fresh durable snapshot so the dashboard can update its
            // state without synchronously reloading the database while it is
            // applying the worker result.
            let mut controller = Controller::load()?;
            super::retain_workspace_sessions(&mut controller, &workspace_id, &client_id)?;
            Ok(controller)
        },
        move |result| DashboardIoUpdate::ContainerSettings { session_id, result },
    );
}

/// Persist everything one detach produces: the read receipt and the unsent
/// draft. They describe the same moment and the same row, so one task keeps
/// them together and gives the quit path a single handle to await.
pub(crate) fn spawn_detached_session_state_persist(
    client_id: String,
    workspace_id: String,
    session_id: String,
    event_ordinal: u64,
    draft: DetachedSessionDraft,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let persisted_session_id = session_id.clone();
    spawn_critical_async(
        tracker,
        format!("saving draft for {}", short_id(&session_id)),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .persist_detached_session_state(
                    client_id,
                    workspace_id,
                    persisted_session_id,
                    event_ordinal,
                    std::process::id(),
                    draft,
                )
                .await
        },
        move |result| DashboardIoUpdate::DetachedSessionState { session_id, result },
    )
}

pub(crate) fn spawn_read_receipt_persist(
    client_id: String,
    workspace_id: String,
    session_id: String,
    through: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let persisted_session_id = session_id.clone();
    spawn_critical_async(
        tracker,
        format!("saving read status for {}", short_id(&session_id)),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .persist_read_receipt(client_id, workspace_id, persisted_session_id, through)
                .await
        },
        move |result| DashboardIoUpdate::ReadReceipt { session_id, result },
    );
}

pub(crate) fn spawn_clipboard_read(updates: UnboundedSender<DashboardIoUpdate>) -> JoinHandle<()> {
    spawn_io(
        "read clipboard",
        updates,
        mj_chat::hel_clipboard::read_text,
        DashboardIoUpdate::ClipboardText,
    )
}

/// Writes copied text to the desktop clipboard off the render loop.
pub(crate) fn spawn_clipboard_write(
    text: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_io(
        "write clipboard",
        updates,
        move || mj_chat::hel_clipboard::write_text(&text),
        DashboardIoUpdate::ClipboardWritten,
    )
}

pub(crate) fn spawn_create_bundle(
    source: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    spawn_critical_io(
        tracker,
        "creating bundle",
        updates,
        move || {
            // Load fresh so a concurrent background save (e.g. an import
            // apply) is not clobbered by a stale UI-time config snapshot.
            let created = mj_controller::hel_controller::create_quick_bundle(&source)?;
            Ok(CreatedBundleUpdate {
                config: created.config,
                bundle_id: created.bundle_id,
            })
        },
        |result| DashboardIoUpdate::CreatedBundle {
            result: Box::new(result),
        },
    );
}

pub(crate) fn spawn_imported_session_apply(
    mut imported: DashboardImportSuccess,
    pending: PendingDashboardImport,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    spawn_critical_io(
        tracker,
        "saving imported session",
        updates,
        move || {
            let session = imported
                .controller
                .state
                .sessions
                .remove(&imported.session_id)
                .context("import worker did not return its new session")?;
            let bundle = imported
                .controller
                .config
                .bundles
                .get(&session.bundle_id)
                .cloned()
                .context("import worker did not return its session bundle")?;
            HelConfig::update(|config| {
                if let Some(existing) = config.bundles.get(&session.bundle_id) {
                    anyhow::ensure!(
                        existing == &bundle,
                        "bundle {:?} changed during import; retry the import",
                        session.bundle_id
                    );
                } else {
                    config
                        .bundles
                        .insert(session.bundle_id.clone(), bundle.clone());
                }
                Ok(())
            })?;
            persist_imported_session(&session)?;
            Ok(ImportedDashboardSessionApply {
                harness: imported.harness,
                native_session_id: pending.native_session_id,
                bundle_id: session.bundle_id.clone(),
                bundle,
                session,
            })
        },
        |result| DashboardIoUpdate::ImportedSessionApplied {
            result: Box::new(result),
        },
    );
}

pub(crate) fn checkpoint_archive_targets(controller: &Controller) -> BTreeMap<String, PathBuf> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| session.state == SessionState::Stopped)
        .filter_map(|session| {
            session
                .checkpoint
                .as_ref()
                .map(|checkpoint| (session.id.clone(), checkpoint.archive_path.clone()))
        })
        .collect()
}

pub(crate) fn spawn_checkpoint_archive_size_refresh(
    generation: u64,
    targets: BTreeMap<String, PathBuf>,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    tokio::task::spawn_blocking(move || {
        let sizes = targets
            .into_iter()
            .map(|(session_id, path)| {
                let size = match std::fs::metadata(&path) {
                    Ok(metadata) => Some(metadata.len()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "could not read checkpoint archive size");
                        None
                    }
                };
                (session_id, size)
            })
            .collect();
        if let Err(error) =
            updates.send(DashboardIoUpdate::CheckpointArchiveSizes { generation, sizes })
        {
            tracing::debug!(generation, %error, "checkpoint archive size result dropped after dashboard shutdown");
        }
    });
}

/// Registering a session and provisioning it are one job with two answers: the
/// dashboard shows the session as soon as it exists, then follows the launch,
/// so this stays separate from [`spawn_lifecycle_operation`].
pub(crate) fn spawn_dashboard_create_session(
    action: DashboardAction,
    workspace_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    lifecycle_updates: UnboundedSender<LifecycleUpdate>,
    runtime: tokio::runtime::Handle,
    tracker: CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable("creating session", cancelled.clone());
    tokio::task::spawn_blocking(move || {
        let generation = match &action {
            DashboardAction::CreateStartupSession { generation, .. } => *generation,
            _ => None,
        };
        let initial_prompt = match &action {
            DashboardAction::CreateStartupSession { initial_prompt, .. } => initial_prompt.clone(),
            _ => None,
        };
        let prepared = match action {
            DashboardAction::CreateStartupSession {
                profile_id,
                target_template_id,
                project_directory,
                ..
            } => super::startup::prepare_session_launch(
                profile_id,
                target_template_id,
                project_directory,
                workspace_id.clone(),
                &cancelled,
            )
            .and_then(|(config, action)| {
                updates
                    .send(DashboardIoUpdate::StartupConfig(config))
                    .context("dashboard closed during startup preparation")?;
                Ok(action)
            }),
            action => Ok(action),
        };
        let action = match prepared {
            Ok(action) => action,
            Err(error) => {
                if let Err(error) = updates.send(DashboardIoUpdate::CreateSession(Box::new(
                    DashboardCreateSessionUpdate::Failed {
                        generation,
                        error: format!("{error:#}"),
                    },
                ))) {
                    tracing::debug!(%error, "startup preparation result dropped after dashboard shutdown");
                }
                return;
            }
        };
        let DashboardAction::CreateSession {
            workspace_id,
            profile_id,
            bundle_id,
            project_directory,
            target_template_id,
            additional_mounts,
            allow_dirty_local,
            resource_allocation,
        } = action.clone()
        else {
            return;
        };
        let registered = (|| -> Result<Option<RegisteredDashboardSession>> {
            let controller = Controller::load()?;
            if !allow_dirty_local && project_directory.is_none() {
                let dirty = controller
                    .config
                    .bundles
                    .get(&bundle_id)
                    .with_context(|| format!("unknown bundle {bundle_id:?}"))
                    .and_then(hel::hel_local_git::dirty_local_repositories)?;
                if !dirty.is_empty() {
                    let repositories = dirty
                        .into_iter()
                        .map(|repository| {
                            format!("{}: {}", repository.path.display(), repository.summary)
                        })
                        .collect();
                    if let Err(error) = updates.send(DashboardIoUpdate::CreateSession(Box::new(
                        DashboardCreateSessionUpdate::DirtyLocal {
                            action,
                            repositories,
                        },
                    ))) {
                        tracing::debug!(%error, "dirty repository result dropped after dashboard shutdown");
                    }
                    return Ok(None);
                }
            }
            if cancelled.load(Ordering::Acquire) {
                bail!("operation cancelled");
            }
            let title = format!(
                "{} via {profile_id}",
                project_directory
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| bundle_id.clone())
            );
            let registered = runtime.block_on(async {
                daemon::connect_or_start()
                    .await?
                    .start_create_session(daemon::CreateSessionRequest {
                        initial_prompt: initial_prompt.clone(),
                        workspace_id,
                        profile_id,
                        bundle_id,
                        project_directory,
                        target_template_id,
                        additional_mounts,
                        allow_dirty_local,
                        resource_allocation,
                        title,
                        session_title_override: None,
                    })
                    .await
            })?;
            Ok(Some(RegisteredDashboardSession {
                generation,
                session: registered.session,
                remembered_container_size: registered.remembered_container_size,
                cancelled: cancelled.clone(),
            }))
        })();
        let Some(registered) = (match registered {
            Ok(registered) => registered,
            Err(error) => {
                if let Err(error) = updates.send(DashboardIoUpdate::CreateSession(Box::new(
                    DashboardCreateSessionUpdate::Failed {
                        generation,
                        error: format!("{error:#}"),
                    },
                ))) {
                    tracing::debug!(%error, "session creation failure dropped after dashboard shutdown");
                }
                None
            }
        }) else {
            return;
        };
        let session_id = registered.session.id.clone();
        if let Err(error) = updates.send(DashboardIoUpdate::CreateSession(Box::new(
            DashboardCreateSessionUpdate::Registered(Box::new(registered)),
        ))) {
            tracing::debug!(%error, "registered session result dropped after dashboard shutdown");
        }
        let result = runtime
            .block_on(async {
                let mut daemon = daemon::connect_or_start().await?;
                daemon.wait_create_session(session_id.clone()).await?;
                if let Some(prompt) = initial_prompt {
                    let result = async {
                        daemon
                            .submit_session_command(
                                session_id.clone(),
                                mj_controller::hel_session_manager::new_command_id(
                                    "initial-prompt",
                                )?,
                                hel::hel_worker::RelayCommand::Prompt {
                                    prompt: vec![
                                        agent_client_protocol::schema::v1::ContentBlock::from(
                                            prompt.as_str(),
                                        ),
                                    ],
                                },
                                Some(prompt),
                            )
                            .await
                            .map(|_| ())
                    }
                    .await;
                    if let Err(error) = result {
                        return Ok(LifecycleSuccess::CreatedWithPromptFailure(format!(
                            "{error:#}"
                        )));
                    }
                }
                Ok::<_, anyhow::Error>(LifecycleSuccess::Created)
            })
            .map_err(|error| format!("{error:#}"));
        if let Err(error) = lifecycle_updates.send(LifecycleUpdate {
            session_id,
            result,
            deferred_cleanup: false,
        }) {
            tracing::debug!(%error, "session creation lifecycle result dropped after dashboard shutdown");
        }
        drop(guard);
    });
}

impl DashboardContext {
    /// Folds one finished background job into dashboard and controller state.
    pub(super) fn apply_dashboard_io_update(&mut self, update: DashboardIoUpdate) {
        match update {
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
            DashboardIoUpdate::WorkspaceManagement { generation, result } => match result {
                Ok(result) => {
                    let WorkspaceManagementResult {
                        revision,
                        entries,
                        select_workspace,
                        deleted_workspace_id,
                    } = result;
                    // A management response may precede an already queued
                    // runtime feed message. Do not let that older message
                    // remove a newly created tab or resurrect a deleted one.
                    let names_are_current = revision >= self.runtime_state_revision;
                    self.runtime_state_revision = self.runtime_state_revision.max(revision);
                    let names: BTreeMap<String, String> = entries
                        .iter()
                        .map(|entry| (entry.workspace.id.clone(), entry.workspace.name.clone()))
                        .collect();
                    if let Some(deleted_workspace_id) = deleted_workspace_id.as_ref() {
                        self.pane_size_persistence.forget(deleted_workspace_id);
                        self.known_workspace_layouts.remove(deleted_workspace_id);
                    }
                    // The TUI owns the modal generation guard. It returns
                    // whether this result still belongs to the visible
                    // manager, so a late create cannot switch another tab.
                    let foreground = self
                        .dashboard
                        .finish_workspace_management(generation, Ok(entries));
                    let next_workspace = deleted_workspace_id
                        .as_ref()
                        .and_then(|deleted_id| names.keys().find(|id| *id != deleted_id).cloned());
                    if names_are_current {
                        self.dashboard.set_workspace_names(names);
                    }
                    if foreground
                        && let Some(workspace_id) = select_workspace
                        && (names_are_current
                            || self.known_workspace_layouts.contains(&workspace_id))
                    {
                        self.dashboard.cancel_modal();
                        self.select_workspace(Some(workspace_id));
                    } else if let Some(deleted_workspace_id) = deleted_workspace_id
                        && self.dashboard.active_workspace_id()
                            == Some(deleted_workspace_id.as_str())
                    {
                        self.select_workspace(next_workspace);
                    }
                }
                Err(error) => {
                    self.dashboard
                        .finish_workspace_management(generation, Err(error));
                }
            },
            DashboardIoUpdate::StartupConfig(config) => {
                self.controller.config = config.clone();
                self.dashboard.set_config(config);
            }
            DashboardIoUpdate::ReviewRefused {
                session_id,
                message,
            } => {
                match self
                    .active_chat
                    .as_mut()
                    .filter(|chat| chat.session_id() == session_id)
                {
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
                            detail,
                            updated_at,
                        },
                        Ok(WorkerRecordPersistenceOutcome::TargetMissing(state)),
                    ) => {
                        if let Some(session) = self.controller.state.sessions.get_mut(&session_id)
                            && matches!(
                                session.state,
                                SessionState::Provisioning
                                    | SessionState::Running
                                    | SessionState::Disconnected
                                    | SessionState::Error
                            )
                        {
                            session.state = state;
                            session.last_error = Some(detail);
                            session.updated_at = updated_at;
                            self.dashboard.set_state(self.controller.state.clone());
                            self.drop_warm_chat_for(&session_id);
                            self.refresh_poll_targets();
                            let notice = match state {
                                SessionState::Error => format!(
                                    "Session {} cannot reach its managed target; its last verified checkpoint is ready to resume",
                                    short_id(&session_id)
                                ),
                                SessionState::Lost => format!(
                                    "Session {} is lost because its managed target no longer exists",
                                    short_id(&session_id)
                                ),
                                _ => unreachable!("a missing target persisted as {state:?}"),
                            };
                            self.dashboard.set_notice(notice);
                        }
                    }
                    (WorkerRecordPersistence::TargetMissing { session_id, .. }, Err(error)) => {
                        self.dashboard.set_notice(format!(
                            "Could not record missing target for {}: {error}",
                            short_id(&session_id)
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
            DashboardIoUpdate::HiddenNativeSessions { result } => match result {
                Ok(hidden) => self.dashboard.set_hidden_native_sessions(hidden),
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not read archived sessions: {error}")),
            },
            DashboardIoUpdate::ArchiveWrite {
                what,
                target,
                result,
            } => {
                if let Err(error) = result {
                    self.dashboard
                        .set_notice(format!("Could not archive {what}: {error}"));
                    // The optimistic update no longer matches storage, so the
                    // row it moved goes back where storage still has it.
                    if revert_archive_write(
                        &target,
                        &mut self.controller.state,
                        &mut self.dashboard,
                    ) {
                        spawn_hidden_native_sessions_load(self.dashboard_io_tx.clone());
                    }
                    self.dirty = true;
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
                session_id,
                result,
            } => {
                // Ignore a late result after a newer request has taken its
                // place (or the dashboard has shut down).
                if !self
                    .attachment
                    .accepts(generation, self.dashboard.selected_session_id())
                    || self.opening_chat_session.as_deref() != Some(session_id.as_str())
                {
                    return;
                }
                self.opening_chat_session = None;
                self.dashboard.set_opening_session(None);
                // The attach may have crossed a lifecycle boundary while it
                // was preparing. Keep the warm chat/draft untouched and drop
                // the late result instead of reviving a retiring conversation.
                if self.dashboard.transition_kind(&session_id).is_some()
                    || self
                        .dashboard
                        .transition_failure_kind(&session_id)
                        .is_some()
                {
                    self.dashboard.set_current_session(None);
                    self.defer_chat_open();
                    self.dirty = true;
                    return;
                }
                match *result {
                    Ok(chat) => {
                        // The old warm chat continued receiving feed updates
                        // while this attach was in flight. Capture and persist
                        // its latest local composer just before replacing it.
                        if let Some(ordinal) = self
                            .active_chat
                            .as_ref()
                            .map(mj_chat::hel_chat::ActiveChat::latest_event_ordinal)
                        {
                            self.record_detach(ordinal);
                        }
                        self.save_active_question_draft();
                        let chat = if let Some(draft) = self.composer_drafts.get(&session_id) {
                            chat.with_draft(draft.text.clone())
                        } else {
                            chat
                        };
                        let mut chat = chat.open_replacing(self.active_chat.as_ref());
                        self.restore_question_draft(&session_id, &mut chat);
                        self.active_chat = Some(chat);
                        // The context travelled with the attach, which is
                        // asynchronous; anything the surface learned while it
                        // was in flight is handed over now.
                        self.refresh_chat_context();
                        self.apply_runtime_review_to_active_chat();
                        self.dashboard.set_current_session(Some(&session_id));
                        self.dashboard.clear_notice();
                        self.acknowledge_visible_chat();
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, %error, "could not open session");
                        self.dashboard.set_notice(format!("Could not open session: {error}. Press Enter in Sessions to retry, or select another session. Alt-Q quits."));
                    }
                }
                self.dirty = true;
            }
            DashboardIoUpdate::CreateSession(update) => self.apply_create_session_update(*update),
            DashboardIoUpdate::RenameSession {
                session_id,
                title,
                result,
            } => match result {
                Ok(title) => {
                    if let Some(session) = self.controller.state.sessions.get_mut(&session_id) {
                        session.session_title_override = Some(title.clone());
                        session.updated_at = chrono::Utc::now().to_rfc3339();
                    }
                    self.dashboard.set_state(self.controller.state.clone());
                    self.dashboard
                        .set_notice(format!("Renamed session to {title}"));
                }
                Err(error) => {
                    self.dashboard
                        .set_notice(format!("Rename failed for {title}: {error}"));
                }
            },
            DashboardIoUpdate::ContainerSettings { session_id, result } => match result {
                Ok(controller) => {
                    self.controller = controller;
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.refresh_chat_context();
                    self.dashboard.set_notice(format!(
                        "Container settings saved for {}; applies when it is next recreated.",
                        short_id(&session_id)
                    ));
                }
                Err(error) => self.dashboard.set_notice(format!(
                    "Container settings failed for {}: {error}",
                    short_id(&session_id)
                )),
            },
            DashboardIoUpdate::TargetTest { target_id, result } => {
                self.target_test_cancel = None;
                self.dashboard.apply_target_test(target_id, result);
            }
            DashboardIoUpdate::ConfigRename { what, result } => match result {
                Ok(controller) => {
                    self.controller = controller;
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.refresh_chat_context();
                    self.refresh_poll_targets();
                    self.request_quota_refresh();
                    self.dashboard.set_notice(format!("Renamed {what}."));
                }
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not rename {what}: {error}")),
            },
            DashboardIoUpdate::ConfigReloaded(result) => {
                self.config_reload_in_flight = false;
                match result {
                    Ok(controller) => {
                        self.controller = controller;
                        self.dashboard.set_config(self.controller.config.clone());
                        self.dashboard.set_state(self.controller.state.clone());
                        self.refresh_chat_context();
                        self.refresh_poll_targets();
                    }
                    Err(error) => self
                        .dashboard
                        .set_notice(format!("Could not reload configuration: {error}")),
                }
            }
            DashboardIoUpdate::WebAccess { generation, access } => {
                if generation == self.web_request_generation {
                    self.dashboard.apply_web_access(access);
                }
            }
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
            DashboardIoUpdate::SetupDiscovered { generation, result } => {
                self.dashboard.setup_discovered(generation, result)
            }
            DashboardIoUpdate::SetupSaved { generation, result } => {
                if let Ok(config) = &result {
                    self.controller.config = config.clone();
                    self.refresh_chat_context();
                    self.request_quota_refresh();
                    self.refresh_poll_targets();
                }
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
                    self.controller.config = config.clone();
                    self.dashboard.set_config(config);
                    self.refresh_chat_context();
                    self.dashboard.set_notice(format!(
                        "Spinner: {style}. F2 → Next spinner style to change it."
                    ));
                }
                Err(error) => {
                    self.dashboard.finish_spinner_style_save();
                    self.dashboard
                        .set_failure_notice(format!("Could not save spinner style: {error}"));
                }
            },
            DashboardIoUpdate::ReviewSettingsSaved { result } => match result {
                Ok(config) => {
                    self.review_discovery_cancel = None;
                    self.controller.config = config.clone();
                    self.dashboard.set_config(config);
                    self.refresh_chat_context();
                    self.dashboard.cancel_modal();
                    self.dashboard
                        .set_notice("Review settings saved; they apply to subsequent reviews.");
                }
                Err(error) => {
                    self.dashboard.review_settings_save_failed(error.clone());
                    self.dashboard
                        .set_notice(format!("Could not save review settings: {error}"));
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
                        short_id(&session_id)
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
                    self.controller.config = created.config;
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
                    self.dashboard
                        .set_notice(format!("Could not create bundle: {error}"));
                }
            },
            DashboardIoUpdate::ImportedSessionApplied { result } => match *result {
                Ok(applied) => {
                    let session_id = applied.session.id.clone();
                    self.controller
                        .config
                        .bundles
                        .insert(applied.bundle_id, applied.bundle);
                    self.controller
                        .state
                        .sessions
                        .insert(session_id.clone(), applied.session);
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.resolve_project_sources();
                    self.refresh_poll_targets();
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
                Err(error) => self.dashboard.set_notice(format!("Import failed: {error}")),
            },
            DashboardIoUpdate::LifecycleReloaded(reloaded) => {
                self.apply_lifecycle_reloaded(*reloaded)
            }
            DashboardIoUpdate::LifecycleCancellation { session_id, result } => {
                if let Err(error) = result {
                    self.dashboard.set_failure_notice(format!(
                        "Could not cancel operation for {}: {error}",
                        short_id(&session_id)
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
            DashboardIoUpdate::CheckpointArchiveSizes { generation, sizes } => {
                if generation == self.checkpoint_archive_generation {
                    self.dashboard.apply_checkpoint_archive_sizes(sizes);
                }
            }
            DashboardIoUpdate::WorkerDiagnosis {
                session_id,
                episode_id,
                result,
            } => self.apply_worker_diagnosis(session_id, episode_id, result),
            DashboardIoUpdate::MountCompletions { prefix, result } => match result {
                Ok(candidates) => self
                    .dashboard
                    .apply_mount_source_completions(&prefix, candidates),
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Path completion failed: {error}")),
            },
            DashboardIoUpdate::MountValidation { source, result } => {
                let action = self
                    .dashboard
                    .apply_mount_source_validation(&source, result);
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
                                self.dashboard.set_notice(format!(
                                    "Could not check checkpoint repositories: {error:#}"
                                ));
                            }
                        }
                        launch => {
                            self.dashboard.finish_session_mount_preflight();
                            super::actions::start_session_launch(self, launch);
                        }
                    },
                    Ok(Some((source, error))) => {
                        self.dashboard
                            .apply_session_mount_preflight_failure(&source, error);
                    }
                    Err(error) => self
                        .dashboard
                        .set_notice(format!("Could not check attached directories: {error}")),
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
                    Ok(applied) => {
                        if let Some(config) = applied.config {
                            self.controller.config = config.clone();
                            self.dashboard.set_config(config);
                        }
                        match applied.preflight {
                            ResumeRepositorySourcePreflight::Ready(receipt) => {
                                self.dashboard.finish_resume_repository_preflight();
                                super::actions::start_preflighted_session_launch(
                                    self, *launch, receipt,
                                );
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
                        }
                    }
                    Err(error) => {
                        if let Some(repository_id) = submitted_repository_id {
                            self.dashboard
                                .apply_repository_origin_failure(&repository_id, error);
                        } else {
                            self.dashboard.set_notice(format!(
                                "Could not check checkpoint repositories: {error}"
                            ));
                        }
                    }
                }
            }
            DashboardIoUpdate::ProjectValidation { directory, result } => self
                .dashboard
                .apply_project_directory_validation(&directory, result),
        }
    }

    fn apply_create_session_update(&mut self, update: DashboardCreateSessionUpdate) {
        match update {
            DashboardCreateSessionUpdate::DirtyLocal {
                action,
                repositories,
            } => self
                .dashboard
                .show_dirty_local_confirmation(action, repositories),
            DashboardCreateSessionUpdate::Registered(registered) => {
                let registered = *registered;
                self.dashboard.finish_quick_new(registered.generation);
                let session_id = registered.session.id.clone();
                if let Some((host, size)) = registered.remembered_container_size {
                    self.controller.state.remember_container_size(&host, size);
                }
                self.controller
                    .state
                    .sessions
                    .insert(session_id.clone(), registered.session);
                self.dashboard.set_state(self.controller.state.clone());
                self.resolve_project_sources();
                self.dashboard.begin_session_operation(
                    session_id.clone(),
                    SessionOperationKind::Launching,
                    None,
                );
                self.dashboard
                    .set_notice(format!("Launching {}…", short_id(&session_id)));
                self.lifecycle_operations.insert(
                    session_id,
                    ActiveLifecycleOperation {
                        cancelled: registered.cancelled,
                        kind: SessionOperationKind::Launching,
                    },
                );
            }
            DashboardCreateSessionUpdate::Failed { generation, error } => {
                self.dashboard
                    .quick_new_failed(generation, format!("Could not create session: {error}"));
            }
        }
    }

    fn apply_lifecycle_reloaded(&mut self, reloaded: LifecycleReloaded) {
        let LifecycleReload { update, operation } = reloaded.reload;
        let session_id = update.session_id;
        let loaded = match reloaded.result {
            Ok(loaded) => loaded,
            Err(error) => {
                self.dashboard
                    .set_notice(format!("Could not reload completed operation: {error}"));
                return;
            }
        };
        self.controller = loaded;
        self.dashboard.set_state(self.controller.state.clone());
        self.resolve_project_sources();
        // A lifecycle may finish after the user changed tabs. Its durable
        // record still belongs in the global controller, but completion must
        // not move the visible selection or replace another workspace's chat.
        let focus_session = self.session_in_active_workspace(&session_id);
        if update.result.is_ok() {
            self.drop_warm_chat_for(&session_id);
        }
        match update.result {
            Ok(LifecycleSuccess::Created) => {
                if focus_session {
                    self.dashboard.select_active_session(&session_id);
                    self.dashboard.focus_prompt();
                }
                self.dashboard
                    .set_notice(format!("Session {} is ready", short_id(&session_id)));
                self.request_quota_refresh();
            }
            Ok(LifecycleSuccess::CreatedWithPromptFailure(error)) => {
                if focus_session {
                    self.dashboard.select_active_session(&session_id);
                    self.dashboard.focus_prompt();
                }
                self.dashboard.set_failure_notice(format!("Session is ready, but its initial task could not be completed: {error}. Check the transcript before retrying the saved draft."));
                self.request_quota_refresh();
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
                if focus_session {
                    self.request_transcript_tail_seed(&session_id);
                    self.open_chat_session(&session_id);
                }
                self.dashboard.set_notice(format!(
                    "Resumed {} with {profile_id} on {target_id}",
                    short_id(&session_id)
                ));
                self.request_quota_refresh();
            }
            Ok(LifecycleSuccess::Moved(outcome)) => {
                if focus_session {
                    self.request_transcript_tail_seed(&session_id);
                    self.open_chat_session(&session_id);
                }
                let destination = format!("{}/{}", outcome.profile_id, outcome.target_template_id);
                self.dashboard
                    .set_notice(if outcome.outcome == "unchanged" {
                        format!(
                            "Move of {} unchanged on {destination} (operation {})",
                            short_id(&session_id),
                            outcome.operation_id
                        )
                    } else {
                        format!(
                            "Moved {} to {destination}; ready and idle (operation {})",
                            short_id(&session_id),
                            outcome.operation_id
                        )
                    });
                self.request_quota_refresh();
            }
            Ok(LifecycleSuccess::Closed) => {
                self.dashboard
                    .set_notice(format!("Stopped {}", short_id(&session_id)));
            }
            Ok(LifecycleSuccess::ForceStopped) => self.dashboard.set_notice(format!(
                "Force-stopped {} at its latest recovery archive",
                short_id(&session_id)
            )),
            Ok(LifecycleSuccess::DestroyedStopped) => self.dashboard.set_notice(format!(
                "Permanently destroyed stopped session {}",
                short_id(&session_id)
            )),
            Ok(LifecycleSuccess::ForceDestroyed) => self.dashboard.set_notice(format!(
                "Permanently destroyed session {}",
                short_id(&session_id)
            )),
            Err(error) => {
                if operation
                    .as_ref()
                    .is_some_and(|operation| operation.kind == SessionOperationKind::Stopping)
                {
                    self.dashboard.show_close_failure(session_id.clone(), error);
                } else {
                    let label = operation
                        .as_ref()
                        .map_or("Operation", |operation| operation.kind.label());
                    self.dashboard
                        .set_failure_notice(format!("{label} failed: {error}"));
                }
            }
        }
        self.refresh_poll_targets();
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
            self.dashboard
                .set_notice(format!("Session {}: {message}", short_id(&session_id)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_config::ProjectRepository;
    use mj_controller::hel_controller::create_quick_bundle_in_config as create_quick_bundle;

    #[test]
    fn setup_save_merges_unrelated_edits_and_refuses_conflicts_or_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = HelConfig::default();
        original.save_to(&path).unwrap();
        let mut edited = original.clone();
        edited.sessions_side = hel::hel_config::SessionsSide::Right;
        HelConfig::update_to(&path, |current| {
            current.startup.prompt = false;
            Ok(())
        })
        .unwrap();
        let original_json = serde_json::to_string(&original).unwrap();
        let saved = save_setup_at(
            &path,
            &original_json,
            &serde_json::to_string(&edited).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.sessions_side, hel::hel_config::SessionsSide::Right);
        assert!(!saved.startup.prompt);
        assert_eq!(HelConfig::load_from(&path).unwrap(), saved);

        let mut conflicting = original.clone();
        conflicting.phone.bind = "127.0.0.1:1234".parse().unwrap();
        HelConfig::update_to(&path, |current| {
            current.phone.bind = "127.0.0.1:5678".parse().unwrap();
            Ok(())
        })
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            save_setup_at(
                &path,
                &original_json,
                &serde_json::to_string(&conflicting).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("another client")
        );
        let mut invalid = original.clone();
        invalid.startup.profile = Some("missing".into());
        assert!(
            save_setup_at(
                &path,
                &original_json,
                &serde_json::to_string(&invalid).unwrap()
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

    #[test]
    fn quick_github_bundle_uses_collision_suffix_and_reuses_matching_source() {
        let mut config = HelConfig::default();
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

    fn archivable_session(id: &str) -> SessionRecord {
        SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: id.into(),
            title: "Raise the dead".into(),
            harness_kind: HarnessKind::Codex,
            last_profile: "codex-1".into(),
            bundle_id: "hel".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Stopped,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: Some("Raise the dead".into()),
            created_at: "2026-08-14T00:00:00Z".into(),
            updated_at: "2026-08-14T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }

    /// An archive write that never reached the database must not leave the
    /// dashboard and the controller holding a row the database still shows.
    /// Both in-memory copies go back, and the row returns to the dialog.
    #[test]
    fn a_failed_session_archive_write_puts_both_copies_of_the_record_back() {
        let mut state = HelState::default();
        state
            .sessions
            .insert("session-1".into(), archivable_session("session-1"));
        let mut dashboard =
            hel_tui::DashboardState::new(HelConfig::default(), state.clone(), BTreeMap::new());
        dashboard.show_resume_dialog(1, Vec::new());

        // What pressing `a` applies before the write is even scheduled.
        dashboard.set_session_archived("session-1", true);
        state
            .sessions
            .get_mut("session-1")
            .expect("the session")
            .archived = true;

        let reload_hidden_native = revert_archive_write(
            &ArchiveWriteTarget::Session {
                session_id: "session-1".into(),
                archived: false,
            },
            &mut state,
            &mut dashboard,
        );

        assert!(
            !reload_hidden_native,
            "a hel record is restored from what the write knew, not from the native set"
        );
        assert!(!state.sessions["session-1"].archived);
        // Emptying the list moved focus to Cancel. Return to the restored row
        // before archiving it; discovery must not steal focus from a button.
        dashboard.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::BackTab,
            crossterm::event::KeyModifiers::NONE,
        ));
        // Archiving the row asks for the same write the failed one attempted.
        assert_eq!(
            dashboard.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('a'),
                crossterm::event::KeyModifiers::NONE,
            )),
            DashboardAction::SetSessionArchived {
                session_id: "session-1".into(),
                archived: true,
            }
        );
    }

    /// The native hidden set has no per-row memory to restore, so a failed
    /// hide is repaired by re-reading what the database holds.
    #[test]
    fn a_failed_native_hide_write_asks_for_the_stored_set() {
        let mut state = HelState::default();
        let mut dashboard =
            hel_tui::DashboardState::new(HelConfig::default(), state.clone(), BTreeMap::new());
        assert!(revert_archive_write(
            &ArchiveWriteTarget::HiddenNativeSessions,
            &mut state,
            &mut dashboard,
        ));
    }

    const LIFECYCLE_RELOAD_CHILD: &str = "MJ_TEST_LIFECYCLE_RELOAD_CHILD";

    /// A completed lifecycle is the moment a freshly started container session
    /// first appears, so the reload it schedules is exactly when another
    /// workspace's live sessions would flood the pane. The reloaded controller
    /// must carry this workspace's live sessions and the global stopped
    /// history, and nothing else.
    #[tokio::test]
    async fn a_lifecycle_reload_keeps_sessions_from_all_workspaces() {
        if std::env::var_os(LIFECYCLE_RELOAD_CHILD).is_none() {
            // MJ_DATA_DIR is process-global, so the database-backed half runs
            // alone in an exact child with its own store.
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "dashboard::io::tests::a_lifecycle_reload_keeps_sessions_from_all_workspaces",
                    "--nocapture",
                ])
                .env(LIFECYCLE_RELOAD_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .env("MJ_CONFIG_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated lifecycle reload failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _writer = hel::hel_database::install_isolated_test_writer();

        let mut config = HelConfig::default();
        config.profiles.insert(
            "codex".into(),
            hel::hel_config::HarnessProfile {
                kind: HarnessKind::Codex,
                home: PathBuf::from("/home/dev/.codex"),
                environment: BTreeMap::new(),
                context_window_bytes: None,
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
            hel::hel_config::TargetTemplate::LocalPodman {
                container: hel::hel_config::ContainerTemplate {
                    image: "example.invalid/hel-test:latest".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        );
        config.save().unwrap();

        let database = hel::hel_database::database_path();
        let alpha = hel::hel_database::create_workspace_at(&database, "alpha").unwrap();
        let beta = hel::hel_database::create_workspace_at(&database, "beta").unwrap();
        hel::hel_database::save_session(&lifecycle_session(
            "session-alpha-live",
            &alpha.id,
            SessionState::Running,
        ))
        .unwrap();
        hel::hel_database::save_session(&lifecycle_session(
            "session-alpha-stopped",
            &alpha.id,
            SessionState::Stopped,
        ))
        .unwrap();
        hel::hel_database::save_session(&lifecycle_session(
            "session-beta-live",
            &beta.id,
            SessionState::Running,
        ))
        .unwrap();

        let (updates_tx, mut updates_rx) =
            tokio::sync::mpsc::unbounded_channel::<DashboardIoUpdate>();
        spawn_lifecycle_reload(
            LifecycleReload {
                update: LifecycleUpdate {
                    session_id: "session-beta-live".into(),
                    result: Ok(LifecycleSuccess::Created),
                    deferred_cleanup: false,
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
        let ids = loaded.state.sessions.keys().collect::<BTreeSet<_>>();
        assert_eq!(
            ids,
            BTreeSet::from([
                &"session-beta-live".to_owned(),
                &"session-alpha-live".to_owned(),
                &"session-alpha-stopped".to_owned()
            ]),
            "the sidebar keeps all live sessions and stopped history"
        );
    }

    fn lifecycle_session(id: &str, workspace_id: &str, state: SessionState) -> SessionRecord {
        SessionRecord {
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
