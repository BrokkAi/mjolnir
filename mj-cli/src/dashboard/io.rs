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
use hel::hel_state::{
    HelState, MaterializedSession, ProjectSourceIdentity, SessionRecord, SessionState,
};
use hel::hel_targets::CancellableProcessExecutor;
use hel_tui::{
    DashboardAction, PreparedMaterializedSessionDetail, PreparedMaterializedSessionSummary,
    ReviewSettingsProbeResult, ReviewTargetReadiness, SessionOperationKind, WebViewerAccess,
};
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
        session_id: String,
        result: Box<std::result::Result<mj_chat::hel_chat::ActiveChat, String>>,
    },
    /// The daemon refused a review action. The message is a sentence for the
    /// person who pressed the key, so it goes back to the chat that sent it.
    ReviewRefused {
        session_id: String,
        message: String,
    },
    CreateSession(Box<DashboardCreateSessionUpdate>),
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
    WebAccess(WebViewerAccess),
    SetupReloaded(std::result::Result<Controller, String>),
    ReviewSettingsProbed {
        generation: u64,
        profile_id: String,
        model: Option<String>,
        effort: Option<String>,
        result: std::result::Result<ReviewSettingsProbeResult, String>,
    },
    ReviewSettingsSaved {
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
        launch: Box<DashboardAction>,
        result: std::result::Result<Option<(String, String)>, String>,
    },
    ResumeRepositoryPreflight {
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

pub(crate) struct RegisteredDashboardSession {
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
    Failed(String),
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

/// Discovers the advertised reviewer selectors and actual target readiness in
/// a supervised asynchronous task. A fresh generation is included in the
/// reply; the TUI drops replies for edits that happened after this request.
pub(crate) fn spawn_review_settings_probe(
    control: SessionManagerControl,
    request: mj_controller::hel_review_settings::ReviewProbeRequest,
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> Arc<AtomicBool> {
    let profile_id = request.profile.clone();
    let model = request.model.clone();
    let effort = request.effort.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable("checking review readiness", cancelled.clone());
    let worker_cancelled = cancelled.clone();
    tokio::spawn(async move {
        let result = mj_controller::hel_review_settings::probe_review_settings(
            control,
            request,
            worker_cancelled,
        )
        .await
        .map(|report| ReviewSettingsProbeResult {
            model_choices: report.model_choices,
            effort_choices: report.effort_choices,
            targets: report
                .targets
                .into_iter()
                .map(|target| ReviewTargetReadiness {
                    target: target.target,
                    ready: target.ready,
                    message: target.message,
                })
                .collect(),
        })
        .map_err(|error| format!("{error:#}"));
        if let Err(error) = updates.send(DashboardIoUpdate::ReviewSettingsProbed {
            generation,
            profile_id,
            model,
            effort,
            result,
        }) {
            tracing::debug!(%error, "review settings probe result dropped after dashboard shutdown");
        }
        drop(guard);
    });
    cancelled
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
    let runtime = tokio::runtime::Handle::current();
    spawn_critical_io(
        tracker,
        format!("renaming session {}", short_id(&session_id)),
        updates,
        move || {
            runtime.block_on(async {
                daemon::connect_or_start()
                    .await?
                    .set_session_title(renamed_session_id, requested_title)
                    .await
            })
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
    draft: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let persisted_session_id = session_id.clone();
    let runtime = tokio::runtime::Handle::current();
    spawn_critical_io(
        tracker,
        format!("saving draft for {}", short_id(&session_id)),
        updates,
        move || {
            runtime.block_on(async {
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
            })
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
    let runtime = tokio::runtime::Handle::current();
    spawn_critical_io(
        tracker,
        format!("saving read status for {}", short_id(&session_id)),
        updates,
        move || {
            runtime.block_on(async {
                daemon::connect_or_start()
                    .await?
                    .persist_read_receipt(client_id, workspace_id, persisted_session_id, through)
                    .await
            })
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
            let mut config = Controller::load()?.config;
            config
                .bundles
                .insert(session.bundle_id.clone(), bundle.clone());
            config.save()?;
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
        let DashboardAction::CreateSession {
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
                        workspace_id: workspace_id.clone(),
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
                session: registered.session,
                remembered_container_size: registered.remembered_container_size,
                cancelled: cancelled.clone(),
            }))
        })();
        let Some(registered) = (match registered {
            Ok(registered) => registered,
            Err(error) => {
                if let Err(error) = updates.send(DashboardIoUpdate::CreateSession(Box::new(
                    DashboardCreateSessionUpdate::Failed(format!("{error:#}")),
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
                daemon::connect_or_start()
                    .await?
                    .wait_create_session(session_id.clone())
                    .await
            })
            .map(|()| LifecycleSuccess::Created)
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
            DashboardIoUpdate::ChatOpened { session_id, result } => {
                // Ignore a late result after a newer request has taken its
                // place (or the dashboard has shut down).
                if self.opening_chat_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                self.opening_chat_session = None;
                self.dashboard.set_opening_session(None);
                match *result {
                    Ok(chat) => {
                        let mut chat = chat;
                        // The old warm chat continued receiving feed updates
                        // while this attach was in flight. Capture its latest
                        // local form state just before replacing it.
                        self.save_active_question_draft();
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
                        // Nothing opened, so the compact session list must not
                        // go on claiming a conversation is on screen.
                        self.dashboard.set_current_session(
                            self.active_chat
                                .as_ref()
                                .map(mj_chat::hel_chat::ActiveChat::session_id),
                        );
                        // The startup pick often attaches before the session
                        // manager has adopted the session. That resolves
                        // itself, so it retries quietly rather than reporting
                        // a failure the user can do nothing about.
                        if self.retry_startup_attach(&session_id) {
                            tracing::debug!(
                                %session_id,
                                "startup attach was early, retrying: {error}"
                            );
                        } else {
                            self.dashboard
                                .set_notice(format!("Could not open session: {error}"));
                        }
                    }
                }
                // The selection may have moved on while this attach was in
                // flight; the newest row wins.
                self.open_pending_chat_session();
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
            DashboardIoUpdate::WebAccess(access) => self.dashboard.apply_web_access(access),
            DashboardIoUpdate::SetupReloaded(result) => match result {
                Ok(controller) => {
                    self.controller = controller;
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.refresh_chat_context();
                    self.request_quota_refresh();
                    self.refresh_poll_targets();
                    self.dashboard
                        .set_notice("Setup complete. Press Alt-N to start your first session.");
                }
                Err(error) => {
                    self.dashboard
                        .set_notice(format!("Could not reload setup changes: {error}"));
                }
            },
            DashboardIoUpdate::ReviewSettingsProbed {
                generation,
                profile_id,
                model,
                effort,
                result,
            } => {
                if self.dashboard.apply_review_settings_probe(
                    generation,
                    &profile_id,
                    model.as_deref(),
                    effort.as_deref(),
                    result,
                ) {
                    self.review_probe_cancel = None;
                }
            }
            DashboardIoUpdate::ReviewSettingsSaved { result } => match result {
                Ok(config) => {
                    self.review_probe_cancel = None;
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
            DashboardIoUpdate::DetachedSessionState { session_id, result } => {
                if let Err(error) = result {
                    self.dashboard.set_notice(format!(
                        "Could not save draft and read status for {}: {error}",
                        short_id(&session_id)
                    ));
                }
            }
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
                    if let DashboardAction::ResolveAwsResourceOptions {
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
            DashboardIoUpdate::MountValidation { source, result } => self
                .dashboard
                .apply_mount_source_validation(&source, result),
            DashboardIoUpdate::SessionMountValidation { launch, result } => match result {
                Ok(None) => {
                    self.dashboard.finish_session_mount_preflight();
                    match *launch {
                        DashboardAction::PreflightResumeRepositories { launch } => {
                            if let Err(error) =
                                super::actions::start_resume_repository_preflight(self, launch)
                            {
                                self.dashboard.set_notice(format!(
                                    "Could not check checkpoint repositories: {error:#}"
                                ));
                            }
                        }
                        launch => super::actions::start_session_launch(self, launch),
                    }
                }
                Ok(Some((source, error))) => {
                    self.dashboard
                        .apply_session_mount_preflight_failure(&source, error);
                }
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not check attached directories: {error}")),
            },
            DashboardIoUpdate::ResumeRepositoryPreflight {
                launch,
                submitted_repository_id,
                result,
            } => match *result {
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
            },
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
            DashboardCreateSessionUpdate::Failed(error) => {
                self.dashboard
                    .set_notice(format!("Could not create session: {error}"));
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
        if update.result.is_ok() {
            self.drop_warm_chat_for(&session_id);
        }
        match update.result {
            Ok(LifecycleSuccess::Created) => {
                self.dashboard.select_active_session(&session_id);
                self.dashboard.set_notice(format!(
                    "Session {} is ready; press Enter to open it",
                    short_id(&session_id)
                ));
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
                self.request_transcript_tail_seed(&session_id);
                self.dashboard.select_active_session(&session_id);
                self.dashboard.set_notice(format!(
                    "Resumed {} with {profile_id} on {target_id}",
                    short_id(&session_id)
                ));
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
        // The row is listed again, so archiving it asks for the same write the
        // failed one attempted rather than an unarchive.
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
    async fn a_lifecycle_reload_keeps_other_workspaces_live_sessions_out() {
        if std::env::var_os(LIFECYCLE_RELOAD_CHILD).is_none() {
            // MJ_DATA_DIR is process-global, so the database-backed half runs
            // alone in an exact child with its own store.
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "dashboard::io::tests::a_lifecycle_reload_keeps_other_workspaces_live_sessions_out",
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
                &"session-alpha-stopped".to_owned()
            ]),
            "the pane keeps this workspace's live sessions and the global stopped history"
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
