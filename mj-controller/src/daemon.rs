//! Persistent per-user controller daemon and its authenticated local protocol.

mod session_move;
use crate::controller::move_session::{
    MoveMutationGuard, MoveOutcome, MovePreparation, MoveSelection, MoveSessionRequest,
};
pub use mj_client::daemon::*;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::database::StoreSchemaMismatch;
use crate::recovery_gate::RecoveryObserver;
use crate::targets::{
    CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, ProcessExecutor,
    ProvisionStage, ProvisionStageGuard,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use mj_core::config::Config;
use mj_core::relay::RelayCommand;
use mj_core::state::{RecoveryObservation, SessionRecord, SessionState};
use mj_core::subagent::SubagentRecord;

use crate::controller::{
    Controller, ControllerStoreGuard, SessionLaunchOptions, SessionResumeOptions,
};
use crate::review_host::TurnReviewHost;
use crate::session_manager::{
    ManagedSessionView, RemoteSessionPublisher, RemoteSessionRequest, SessionManagerChannels,
    SessionManagerControl, ViewError, new_command_id, spawn_remote_session_manager,
    spawn_session_manager,
};
#[cfg(test)]
use crate::session_manager::{RelaySessionTarget, RemoteSessionRequests, SessionManagerShutdown};
use crate::worker_upgrade::{WorkerUpgradeObservation, WorkerUpgradeObserver};
use mj_core::workspace::WorkspaceRecord;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::pollers::{
    dashboard_worker_targets, dashboard_worker_targets_excluding, interrupted_close_session_ids,
    reserve_recovery_or_cancel, spawn_image_refresher, spawn_interrupted_close_recovery,
};

// Move preparation now reports whether source state must be recovered without its harness.

/// How long the epilogue is given before the process leaves anyway.
///
/// Every daemon exit -- stop, SIGTERM, idle, a store that moved underneath it
/// -- unwinds through the same epilogue, and every step of it is bounded in
/// practice. This makes "the daemon did not stop" impossible rather than
/// unlikely, and it must stay well inside [`STOP_TIMEOUT`] so a client waiting
/// on a stop sees the exit rather than its own deadline.
const SHUTDOWN_FORCE_EXIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long force destruction waits for a cancelled lifecycle to actually
/// stop before refusing to destroy under it. Cancellation kills the
/// operation's child process groups and unwinds its persistence, which is
/// fast in practice; an operation that outlives this bound is wedged in a
/// way destruction must not paper over.
const FORCE_DESTROY_PREEMPT_TIMEOUT: Duration = Duration::from_secs(8);

/// Cancellation and committing a newly started session are one atomic decision.
#[derive(Clone, Default)]
pub struct CreateSessionControl {
    state: Arc<AtomicU8>,
    pub cancelled: Arc<AtomicBool>,
}

impl CreateSessionControl {
    pub fn request_cancel(&self) -> bool {
        let accepted = self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if accepted {
            self.cancelled.store(true, Ordering::Release);
        }
        accepted
    }

    pub fn grant_commit(&self) -> bool {
        self.state
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn is_cancellable(&self) -> bool {
        self.state.load(Ordering::Acquire) == 0
    }
}

#[derive(Debug, Clone)]
struct Attachment {
    pid: u32,
}

pub struct RuntimeState {
    attachments: Mutex<BTreeMap<String, Attachment>>,
    phone_status: Mutex<WebViewerStatus>,
    pub web_viewer: crate::web_viewer::ViewerControl,
    ever_attached: AtomicBool,
    sessions: Mutex<BTreeMap<String, RuntimeSessionView>>,
    revisions: RuntimeRevisions,
    workspaces_tx: tokio::sync::watch::Sender<Vec<WorkspaceRecord>>,
    session_manager: SessionManagerControl,
    lifecycle: Mutex<BTreeMap<String, ActiveLifecycle>>,
    close_requested: Mutex<BTreeSet<String>>,
    controller: Mutex<Controller>,
    controller_loader: fn() -> Result<Controller>,
    config_mutation: tokio::sync::Mutex<()>,
    recovery_observer: RecoveryObserver,
    worker_upgrade_observer: WorkerUpgradeObserver,
    /// Recent background notices, newest last, with the id of the next one.
    /// Bounded: a surface that never attaches must not make this grow.
    notices: Mutex<VecDeque<RuntimeNotice>>,
    next_notice_id: AtomicU64,
    /// What `[review]` last said, republished by the target refresher.
    review_config: Arc<Mutex<mj_core::config::ReviewConfig>>,
    /// Turn review runs here, in the process that owns every session, so a
    /// review happens whether the terminal, the phone, or nobody is attached.
    review_host: TurnReviewHost,
}

/// One monotonic cursor shared by daemon snapshots and their wake-up feed.
///
/// Allocations can come from independent UI and daemon tasks. Publishing an
/// older allocation after a newer one must not move the watch channel
/// backwards, so publication compares against the last visible cursor.
#[derive(Clone)]
struct RuntimeRevisions {
    allocated: Arc<std::sync::atomic::AtomicU64>,
    published: tokio::sync::watch::Sender<u64>,
}

impl RuntimeRevisions {
    fn new(initial: u64) -> Self {
        let (published, _) = tokio::sync::watch::channel(initial);
        Self {
            allocated: Arc::new(std::sync::atomic::AtomicU64::new(initial)),
            published,
        }
    }

    fn allocate(&self) -> u64 {
        self.allocated.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn publish(&self) -> u64 {
        let revision = self.allocate();
        self.publish_allocated(revision);
        revision
    }

    fn publish_allocated(&self, revision: u64) {
        self.published.send_if_modified(|visible| {
            if revision > *visible {
                *visible = revision;
                true
            } else {
                false
            }
        });
    }

    fn notifier(&self) -> Arc<dyn Fn() + Send + Sync> {
        let revisions = self.clone();
        Arc::new(move || {
            revisions.publish();
        })
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.published.subscribe()
    }

    fn current(&self) -> u64 {
        self.allocated.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleKind {
    Create,
    Close,
    Resume,
    Move,
    ForceStop,
    DestroyStopped,
    ForceDestroy,
    Cleanup,
}

/// Whether a lifecycle has exclusive ownership of the worker target, so the
/// session manager must stop polling it. A graceful close needs the manager's
/// relay lease through checkpointing and sealing; once the durable state says
/// `Destroying`, that lease has been released and target teardown is exclusive.
fn lifecycle_owns_worker_target(kind: LifecycleKind, state: Option<SessionState>) -> bool {
    match kind {
        LifecycleKind::Close => state == Some(SessionState::Destroying),
        LifecycleKind::Move => !matches!(
            state,
            Some(
                SessionState::Running
                    | SessionState::Disconnected
                    | SessionState::Checkpointing
                    | SessionState::Closing
            )
        ),
        _ => true,
    }
}

/// Whether a running lifecycle can still be cancelled. A graceful close has a
/// point of no return: once the durable state says `Destroying`, the verified
/// checkpoint is sealed and the record has already committed to losing its
/// target, so stopping the teardown only strands the target. Every other
/// lifecycle stays cancellable while it runs.
fn lifecycle_cancellable(kind: LifecycleKind, state: Option<SessionState>) -> bool {
    !(kind == LifecycleKind::Close && state == Some(SessionState::Destroying))
}

/// How a stop request has to be carried out, given the durable record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseRoute {
    /// Run the graceful close from the start.
    Graceful,
    /// A previous close stopped partway; finish it from its checkpoint.
    RecoverInterrupted,
    /// Already stopped, but the target still has to be removed.
    DeferredCleanup,
    /// Already stopped with nothing left to do.
    Done,
}

/// A record mid-close with a live target cannot be closed again from the start:
/// its worker socket is gone, so a fresh checkpoint attempt only fails on
/// connect. Recovery finishes it from the checkpoint the first close verified.
fn close_route(session: Option<&SessionRecord>) -> CloseRoute {
    let Some(session) = session else {
        return CloseRoute::Graceful;
    };
    if crate::pollers::is_interrupted_close(session) {
        CloseRoute::RecoverInterrupted
    } else if session.state == SessionState::Stopped {
        if session.target.is_some() {
            CloseRoute::DeferredCleanup
        } else {
            CloseRoute::Done
        }
    } else {
        CloseRoute::Graceful
    }
}

/// The durable state of one record as the locked controller holds it.
fn durable_session_state(controller: &Controller, session_id: &str) -> Option<SessionState> {
    controller
        .state
        .sessions
        .get(session_id)
        .map(|session| session.state)
}

struct ActiveLifecycle {
    operation_id: String,
    create_control: Option<CreateSessionControl>,
    kind: LifecycleKind,
    cancelled: Arc<AtomicBool>,
    started_at_epoch_seconds: u64,
    active_stages: BTreeMap<ProvisionStage, (usize, u64)>,
    /// The workspace a resume is claiming before its durable record changes.
    /// Workspace deletion consults this so it cannot race the claim.
    resume_workspace_id: Option<String>,
    resume_destination: Option<(String, String)>,
    notice: Option<String>,
    request_key: Option<String>,
    _move_guard: Option<MoveMutationGuard>,
    move_source_closed: bool,
    result:
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
}

impl ActiveLifecycle {
    fn is_visible(&self) -> bool {
        let result = self.result.borrow();
        result.is_none()
            || matches!(
                result.as_ref(),
                Some(Ok(DaemonLifecycleResult::DeferredCleanup))
            )
    }

    fn request_cancel(&self) -> bool {
        if let Some(control) = &self.create_control {
            control.request_cancel()
        } else {
            !self.cancelled.swap(true, Ordering::AcqRel)
        }
    }

    fn is_cancellable(&self) -> bool {
        self.result.borrow().is_none()
            && self.create_control.as_ref().map_or_else(
                || !self.cancelled.load(Ordering::Acquire),
                CreateSessionControl::is_cancellable,
            )
    }
}

#[derive(Debug, Clone)]
enum DaemonLifecycleResult {
    Done,
    DeferredCleanup,
    Move(MoveOutcome),
}

impl From<LifecycleKind> for RuntimeLifecycleKind {
    fn from(kind: LifecycleKind) -> Self {
        match kind {
            LifecycleKind::Create => Self::Create,
            LifecycleKind::Close => Self::Close,
            LifecycleKind::Resume => Self::Resume,
            LifecycleKind::Move => Self::Move,
            LifecycleKind::ForceStop => Self::ForceStop,
            LifecycleKind::DestroyStopped => Self::DestroyStopped,
            LifecycleKind::ForceDestroy => Self::ForceDestroy,
            LifecycleKind::Cleanup => Self::Cleanup,
        }
    }
}

impl RuntimeState {
    fn new(
        session_manager: SessionManagerControl,
        controller: Controller,
        recovery_observer: RecoveryObserver,
        worker_upgrade_observer: WorkerUpgradeObserver,
        workspaces: Vec<WorkspaceRecord>,
    ) -> Self {
        Self::new_with_controller_loader(
            session_manager,
            controller,
            recovery_observer,
            worker_upgrade_observer,
            workspaces,
            Controller::load,
        )
    }

    fn new_with_controller_loader(
        session_manager: SessionManagerControl,
        controller: Controller,
        recovery_observer: RecoveryObserver,
        worker_upgrade_observer: WorkerUpgradeObserver,
        workspaces: Vec<WorkspaceRecord>,
        controller_loader: fn() -> Result<Controller>,
    ) -> Self {
        // Revisions are opaque cursors, so give every daemon incarnation a
        // fresh high-water mark. Clients that survive a daemon restart must
        // never wait on, or render, a cursor from the previous process as if
        // it belonged to the new feed.
        let initial_revision = u64::try_from(chrono::Utc::now().timestamp_micros()).unwrap_or(1);
        let revisions = RuntimeRevisions::new(initial_revision);
        let (workspaces_tx, _) = tokio::sync::watch::channel(workspaces);
        // The host reads `[review]` at each trigger decision. The target
        // refresher already reloads config.toml every 500 ms and installs the
        // result here, so arming needs no reload machinery of its own.
        let review_config = Arc::new(Mutex::new(controller.config.review.clone()));
        let review_host = TurnReviewHost::spawn_notifying(
            session_manager.clone(),
            {
                let installed = review_config.clone();
                Arc::new(move || {
                    installed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone()
                })
            },
            revisions.notifier(),
        );
        Self {
            attachments: Mutex::new(BTreeMap::new()),
            phone_status: Mutex::new(WebViewerStatus::Starting),
            web_viewer: crate::web_viewer::ViewerControl::new(),
            ever_attached: AtomicBool::new(false),
            sessions: Mutex::new(BTreeMap::new()),
            revisions,
            workspaces_tx,
            session_manager,
            lifecycle: Mutex::new(BTreeMap::new()),
            close_requested: Mutex::new(BTreeSet::new()),
            controller: Mutex::new(controller),
            controller_loader,
            config_mutation: tokio::sync::Mutex::new(()),
            recovery_observer,
            worker_upgrade_observer,
            notices: Mutex::new(VecDeque::new()),
            next_notice_id: AtomicU64::new(1),
            review_config,
            review_host,
        }
    }

    /// The review host, for the surfaces that project and resolve reviews.
    pub fn review_host(&self) -> &TurnReviewHost {
        &self.review_host
    }

    pub fn allocate_revision(&self) -> u64 {
        self.revisions.allocate()
    }

    fn publish_revision(&self) -> u64 {
        self.revisions.publish()
    }

    fn attachments(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Attachment>> {
        self.attachments
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn prune_dead_clients(&self) {
        self.attachments()
            .retain(|_, attachment| process_is_alive(attachment.pid));
    }

    fn workspace_has_active_resume(&self, workspace_id: &str) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|active| {
                active.result.borrow().is_none()
                    && active.resume_workspace_id.as_deref() == Some(workspace_id)
            })
    }

    pub fn publish_web_access(&self, access: crate::server::WebViewerAccess) {
        use crate::server::WebViewerAccess;
        let status = match &access {
            WebViewerAccess::Starting => WebViewerStatus::Starting,
            WebViewerAccess::Ready {
                viewer_url,
                viewer_code,
                qr_login_url,
                fallback_reason,
            } => WebViewerStatus::Ready {
                viewer_url: viewer_url.clone(),
                viewer_code: viewer_code.clone(),
                qr_login_url: qr_login_url.clone(),
                fallback_reason: fallback_reason.clone(),
            },
            WebViewerAccess::Failed {
                address, message, ..
            } => WebViewerStatus::Error {
                message: format!("{message} Address: {address}"),
            },
            WebViewerAccess::Unavailable(message) => WebViewerStatus::Error {
                message: message.clone(),
            },
        };
        self.web_viewer.publish(access);
        self.set_phone_status(status);
    }

    fn set_phone_status(&self, status: WebViewerStatus) {
        *self
            .phone_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = status;
    }

    fn phone_status(&self) -> WebViewerStatus {
        self.phone_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn workspaces(&self) -> tokio::sync::watch::Receiver<Vec<WorkspaceRecord>> {
        self.workspaces_tx.subscribe()
    }

    fn worker_poll_exclusion_session_ids(&self, controller: &Controller) -> BTreeSet<String> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, active)| {
                active.result.borrow().is_none()
                    && (active.move_source_closed
                        || lifecycle_owns_worker_target(
                            active.kind,
                            controller
                                .state
                                .sessions
                                .get(*session_id)
                                .map(|session| session.state),
                        ))
            })
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    pub fn revisions(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revisions.subscribe()
    }

    /// Read the config the daemon serves right now. A task on a schedule reads
    /// it again on every tick, so a reload reaches it without a restart.
    pub fn with_config<T>(&self, read: impl FnOnce(&Config) -> T) -> T {
        read(
            &self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .config,
        )
    }

    /// Create a bundle under the daemon's config-mutation coordinator. The
    /// controller helper also takes the cross-process config lock, so a TUI
    /// transaction cannot race this one while the daemon's other config
    /// writers are excluded by this mutex.
    pub async fn create_quick_bundle(
        &self,
        source: String,
    ) -> std::result::Result<
        crate::controller::QuickBundleCreation,
        crate::controller::QuickBundleFailure,
    > {
        let _mutation = self.config_mutation.lock().await;
        tokio::task::spawn_blocking(move || crate::controller::create_quick_bundle(&source))
            .await
            .map_err(|error| {
                crate::controller::QuickBundleFailure::Persistence(anyhow!(
                    "bundle creation task panicked: {error}"
                ))
            })?
    }

    fn publish_workspaces(&self, workspaces: Vec<WorkspaceRecord>) {
        self.workspaces_tx.send_replace(workspaces);
        self.publish_revision();
    }

    pub async fn reload_controller(&self) -> Result<()> {
        // Serialize installs so an earlier phone publication cannot overwrite
        // a later completed lifecycle with the controller snapshot it loaded.
        let _mutation = self.config_mutation.lock().await;
        let controller_loader = self.controller_loader;
        let controller = tokio::task::spawn_blocking(controller_loader)
            .await
            .context("daemon controller reload task panicked")??;
        let session_count = controller.state.sessions.len();
        *self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = controller;
        let revision = self.publish_revision();
        tracing::debug!(revision, session_count, "daemon controller state reloaded");
        Ok(())
    }

    /// Definitive missing-target evidence belongs to the daemon, including
    /// when no terminal is attached. Generic connection failures stay transient.
    fn missing_target_record(
        &self,
        session_id: &str,
        view: &ManagedSessionView,
    ) -> Option<(String, String)> {
        let Some(ViewError::TargetMissing(detail)) = &view.error else {
            return None;
        };
        if view.connected {
            return None;
        }
        if self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .is_some_and(|active| active.result.borrow().is_none())
        {
            return None;
        }
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let session = controller.state.sessions.get(session_id)?;
        if !matches!(
            session.state,
            SessionState::Running | SessionState::Disconnected
        ) {
            return None;
        }
        Some((detail.clone(), session.updated_at.clone()))
    }

    async fn persist_missing_target(
        &self,
        session_id: &str,
        detail: String,
        observed_updated_at: String,
    ) -> Result<()> {
        let changed = blocking({
            let session_id = session_id.to_owned();
            let detail = detail.clone();
            move || {
                crate::database::mark_session_target_missing_if_current(
                    &session_id,
                    &detail,
                    &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    &observed_updated_at,
                )
            }
        })
        .await?;
        if changed.is_some() {
            self.reload_controller().await?;
            self.push_notice(session_id, detail);
        }
        Ok(())
    }

    async fn publish_session(&self, session_id: String, view: ManagedSessionView) -> Result<()> {
        let connected = view.connected;
        let has_snapshot = view.snapshot.is_some();
        tracing::debug!(
            %session_id,
            connected,
            has_snapshot,
            "daemon received a session view"
        );
        if let Some(snapshot) = view.snapshot.as_ref() {
            let controller = self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(session) = controller.state.sessions.get(&session_id).cloned() {
                // An upgrade only ever runs on a quiet session, and a session
                // in a turn publishes a view every 150 ms. Skipping those here
                // keeps the config clone off the streaming path; the
                // coordinator still decides, from `quiet`, whether to act.
                let quiet =
                    view.connected && snapshot.operational.safe_to_replace(session.harness_kind);
                if quiet {
                    self.worker_upgrade_observer
                        .observe(WorkerUpgradeObservation {
                            session: session.clone(),
                            config: controller.config.clone(),
                            worker_build: snapshot.worker_build.clone(),
                            quiet,
                        });
                }
                self.recovery_observer.observe(RecoveryObservation {
                    checkpoint_safe: snapshot
                        .operational
                        .safe_for_checkpoint(session.harness_kind),
                    session,
                    config: controller.config.clone(),
                    latest_completed_turn_ordinal: snapshot.latest_completed_turn_ordinal(),
                    execution: snapshot.materialized.execution,
                });
            }
        }
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                session_id.clone(),
                RuntimeSessionView::from_managed(session_id, view),
            );
        reach_test_hook("relay_projection_before_revision_publication").await?;
        self.publish_revision();
        Ok(())
    }

    async fn runtime_snapshot(
        &self,
        workspace_id: &str,
        after_revision: u64,
        all_workspaces: bool,
    ) -> Result<RuntimeSnapshot> {
        let mut revisions = self.revisions.subscribe();
        if *revisions.borrow_and_update() <= after_revision {
            let _ = tokio::time::timeout(Duration::from_secs(30), revisions.changed()).await;
        }
        let revision = self.revisions.current();
        let moves = blocking(crate::database::load_move_operations).await?;
        let workspace_names = blocking(crate::database::list_workspaces)
            .await?
            .into_iter()
            .map(|workspace| (workspace.id, workspace.name))
            .collect();
        let session_ids = if all_workspaces {
            self.controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .state
                .sessions
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
        } else {
            let workspace_id = workspace_id.to_owned();
            blocking(move || crate::database::session_ids_for_workspace(&workspace_id))
                .await?
                .into_iter()
                .collect()
        };
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, _)| session_ids.contains(*session_id))
            .map(|(_, view)| view.clone())
            .collect();
        // Match the controller -> lifecycle lock order used by worker polling.
        // Completion reloads records before publishing its result, so holding
        // this guard prevents an absent operation paired with older records.
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let lifecycles = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, active)| {
                (all_workspaces
                    || session_ids.contains(*session_id)
                    || active.resume_workspace_id.as_deref() == Some(workspace_id))
                    && active.is_visible()
            })
            .map(|(session_id, active)| RuntimeLifecycleView {
                operation_id: active.operation_id.clone(),
                cancellable: active.is_cancellable()
                    && lifecycle_cancellable(
                        active.kind,
                        durable_session_state(&controller, session_id),
                    ),
                session_id: session_id.clone(),
                kind: active.kind.into(),
                started_at_epoch_seconds: active.started_at_epoch_seconds,
                active_stages: active
                    .active_stages
                    .iter()
                    .map(|(stage, (_, started_at))| (*stage, *started_at))
                    .collect(),
                resume_destination: active.resume_destination.clone(),
                notice: active.notice.clone(),
            })
            .collect();
        let reviews = self
            .review_host
            .views()
            .into_iter()
            .filter(|review| session_ids.contains(&review.session_id))
            .collect();
        let notices = self
            .notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|notice| session_ids.contains(&notice.session_id))
            .cloned()
            .collect();
        let records = runtime_records_for_workspace(&controller, &session_ids);
        let subagents = runtime_subagents_for_workspace(&controller, &records);
        Ok(RuntimeSnapshot {
            workspace_names,
            moves: moves
                .into_iter()
                .filter(|operation| session_ids.contains(&operation.selection.session_id))
                .collect(),
            revision,
            config: controller.config.clone(),
            records,
            sessions,
            lifecycles,
            reviews,
            notices,
            subagents,
        })
    }

    fn start_or_join_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_for_workspace(session_id, kind, None, work)
    }

    fn start_or_join_lifecycle_for_workspace<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        work: F,
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_with_key(session_id, kind, resume_workspace_id, None, work)
    }

    fn start_or_join_lifecycle_with_key<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        request_key: Option<String>,
        work: F,
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        self.start_or_join_lifecycle_controlled(
            session_id,
            kind,
            resume_workspace_id,
            request_key,
            None,
            work,
        )
    }

    fn start_or_join_lifecycle_controlled<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        resume_workspace_id: Option<String>,
        request_key: Option<String>,
        create_control: Option<CreateSessionControl>,
        work: F,
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    >
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let mut work = Some(work);
        ensure!(
            matches!(kind, LifecycleKind::Move | LifecycleKind::ForceDestroy)
                || !crate::controller::move_session::move_has_pending_queue(&session_id),
            "Move queue admission is incomplete; retry Move on the same destination before another lifecycle operation"
        );
        let result = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let completed_other_kind = lifecycle
                .get(&session_id)
                .is_some_and(|active| active.kind != kind && active.result.borrow().is_some());
            if completed_other_kind {
                lifecycle.remove(&session_id);
            }
            if let Some(active) = lifecycle.get(&session_id) {
                ensure!(
                    active.request_key == request_key,
                    "another lifecycle request with different selections is already running for session {session_id}"
                );
                ensure!(
                    active.kind == kind,
                    "another lifecycle operation is already running for session {session_id}"
                );
                ensure!(
                    resume_workspace_id.is_none()
                        || active.resume_workspace_id == resume_workspace_id,
                    "session {session_id} is already resuming into another workspace"
                );
                active.result.clone()
            } else {
                let cancelled = create_control
                    .as_ref()
                    .map(|control| control.cancelled.clone())
                    .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                let (result_tx, result_rx) = tokio::sync::watch::channel(None);
                lifecycle.insert(
                    session_id.clone(),
                    ActiveLifecycle {
                        operation_id: new_command_id("lifecycle")?,
                        create_control,
                        kind,
                        cancelled: cancelled.clone(),
                        started_at_epoch_seconds: epoch_seconds(),
                        active_stages: BTreeMap::new(),
                        resume_workspace_id,
                        resume_destination: None,
                        notice: None,
                        request_key,
                        move_source_closed: false,
                        _move_guard: (kind == LifecycleKind::Move)
                            .then(|| MoveMutationGuard::reserve(&session_id))
                            .transpose()?,
                        result: result_rx.clone(),
                    },
                );
                self.publish_revision();
                let state = Arc::clone(self);
                let operation_session_id = session_id.clone();
                let operation = work.take().expect("new lifecycle operation has work");
                let completed_channel = result_rx.clone();
                tokio::spawn(async move {
                    let operation_state = state.clone();
                    let operation_id = operation_session_id.clone();
                    let mut result = match tokio::spawn(async move {
                        operation(operation_state, operation_id, cancelled).await
                    })
                    .await
                    {
                        Ok(result) => result.map_err(|error| format!("{error:#}")),
                        Err(error) => Err(format!("daemon lifecycle task failed: {error}")),
                    };
                    if let Err(error) = state.reload_controller().await {
                        let reload_error = format!(
                            "reload daemon state after lifecycle operation for {operation_session_id}: {error:#}"
                        );
                        if result.is_ok() {
                            result = Err(reload_error);
                        } else {
                            tracing::warn!(
                                session_id = %operation_session_id,
                                error = reload_error,
                                "lifecycle failed and its durable state could not be reloaded"
                            );
                        }
                    }
                    if let Err(error) =
                        reach_test_hook("lifecycle_reservation_before_result_publication").await
                    {
                        result = Err(format!("test lifecycle publication hook failed: {error:#}"));
                    }
                    let deferred_cleanup =
                        matches!(result, Ok(DaemonLifecycleResult::DeferredCleanup));
                    result_tx.send_replace(Some(result));
                    // Completion must release transient mutation ownership even
                    // when every requesting client has disconnected. Durable
                    // partial queue admission has its own independent hold.
                    if let Some(active) = state
                        .lifecycle
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get_mut(&operation_session_id)
                        && active.result.same_channel(&completed_channel)
                    {
                        active._move_guard.take();
                    }
                    // Hand off under daemon ownership even if the requesting
                    // client disconnects. The completed close remains visible
                    // until the cleanup replaces it in the lifecycle map.
                    if deferred_cleanup
                        && let Err(error) =
                            state.start_deferred_cleanup(operation_session_id.clone())
                    {
                        tracing::warn!(session_id = %operation_session_id, %error, "could not start retained cleanup");
                        state.push_notice(&operation_session_id, "Container cleanup could not start; retry cleanup from the stopped session.");
                        state
                            .lifecycle
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .retain(|_, active| !active.result.same_channel(&completed_channel));
                    }
                    state.publish_revision();
                });
                result_rx
            }
        };
        Ok(result)
    }

    async fn wait_lifecycle_result(
        mut result: tokio::sync::watch::Receiver<
            Option<std::result::Result<DaemonLifecycleResult, String>>,
        >,
    ) -> Result<DaemonLifecycleResult> {
        loop {
            if let Some(result) = result.borrow_and_update().clone() {
                return result.map_err(anyhow::Error::msg);
            }
            result
                .changed()
                .await
                .context("daemon lifecycle operation stopped without a result")?;
        }
    }

    async fn run_lifecycle<F, Fut>(
        self: &Arc<Self>,
        session_id: String,
        kind: LifecycleKind,
        work: F,
    ) -> Result<DaemonLifecycleResult>
    where
        F: FnOnce(Arc<Self>, String, Arc<AtomicBool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<DaemonLifecycleResult>> + Send + 'static,
    {
        let result = self.start_or_join_lifecycle(session_id, kind, work)?;
        let channel = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        outcome
    }

    fn remove_completed_lifecycle(
        &self,
        channel: &tokio::sync::watch::Receiver<
            Option<std::result::Result<DaemonLifecycleResult, String>>,
        >,
    ) {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, active| !active.result.same_channel(channel) || active.is_visible());
    }

    async fn start_create_session(
        self: &Arc<Self>,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        self.start_create_session_inner(request, CreateSessionControl::default(), None)
            .await
    }

    /// Register and start a child worker on its parent's existing target.
    pub async fn start_subagent_session(
        self: &Arc<Self>,
        request: crate::controller::RegisterSubagentRequest,
    ) -> Result<mj_core::subagent::SubagentRecord> {
        let relation = blocking(move || {
            let mut controller = Controller::load()?;
            controller.register_subagent(request)
        })
        .await?;
        let session_id = relation.child_session_id.clone();
        self.start_or_join_lifecycle_controlled(
            session_id.clone(),
            LifecycleKind::Create,
            None,
            Some(relation.request_key.clone()),
            None,
            move |state, session_id, cancelled| async move {
                let mut controller = tokio::task::spawn_blocking(Controller::load)
                    .await
                    .context("load controller for sub-agent startup")??;
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state,
                    session_id.clone(),
                );
                controller
                    .provision_subagent_session_controlled(&session_id, &executor)
                    .await?;
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
        self.reload_controller().await?;
        Ok(relation)
    }

    pub async fn start_create_session_controlled(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        control: CreateSessionControl,
        publication: tokio::sync::oneshot::Receiver<std::result::Result<(), String>>,
    ) -> Result<RegisteredSession> {
        self.start_create_session_inner(request, control, Some(publication))
            .await
    }

    async fn start_create_session_inner(
        self: &Arc<Self>,
        request: CreateSessionRequest,
        control: CreateSessionControl,
        publication: Option<tokio::sync::oneshot::Receiver<std::result::Result<(), String>>>,
    ) -> Result<RegisteredSession> {
        let path_cancelled = control.cancelled.clone();
        let registered = blocking(move || {
            let mut controller = Controller::load()?;
            let path_executor = crate::targets::CancellableProcessExecutor::new(path_cancelled)
                .with_deadline(Duration::from_secs(30));
            let project_directory = request
                .project_directory
                .as_deref()
                .map(|path| {
                    controller.resolve_project_directory(
                        &request.target_template_id,
                        path,
                        &path_executor,
                    )
                })
                .transpose()?;
            let session_id = controller.register_session_with_resources(
                &request.profile_id,
                &request.bundle_id,
                &request.target_template_id,
                request.title,
                SessionLaunchOptions {
                    create_managed_worktree: request.create_managed_worktree,
                    mjolnir_subagents: request.mjolnir_subagents,
                    initial_prompt: request.initial_prompt,
                    workspace_id: request.workspace_id,
                    additional_mounts: request.additional_mounts,
                    resource_allocation: request.resource_allocation,
                    project_directory,
                    session_title_override: request.session_title_override,
                },
            )?;
            let session = controller
                .state
                .sessions
                .get(&session_id)
                .expect("newly registered session exists")
                .clone();
            let remembered_container_size = controller
                .config
                .targets
                .get(&request.target_template_id)
                .and_then(mj_core::config::container_size_host)
                .and_then(|host| {
                    controller
                        .state
                        .container_sizes
                        .get(host)
                        .copied()
                        .map(|size| (host.to_owned(), size))
                });
            Ok(RegisteredSession {
                session,
                remembered_container_size,
            })
        })
        .await?;
        let session_id = registered.session.id.clone();
        self.start_or_join_lifecycle_controlled(
            session_id,
            LifecycleKind::Create,
            None,
            None,
            Some(control.clone()),
            move |state, session_id, cancelled| async move {
                let mut controller = tokio::task::spawn_blocking(Controller::load)
                    .await
                    .context("load controller for daemon create task")??;
                let publication_error = if let Some(publication) = publication {
                    let published = tokio::select! {
                        result = publication => result.context("session publication owner stopped")
                            .and_then(|result| result.map_err(anyhow::Error::msg)),
                        () = async {
                            while !cancelled.load(Ordering::Acquire) {
                                tokio::time::sleep(Duration::from_millis(25)).await;
                            }
                        } => Err(anyhow!("session creation cancelled before publication")),
                    };
                    published.err()
                } else {
                    None
                };
                if publication_error.is_some() {
                    control.request_cancel();
                }
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state,
                    session_id.clone(),
                );
                let provision = controller
                    .provision_session_controlled_with_commit(&session_id, &executor, || {
                        ensure!(
                            control.grant_commit(),
                            "session creation cancelled before commit"
                        );
                        Ok(())
                    })
                    .await;
                if let Some(error) = publication_error {
                    return match provision {
                        Ok(()) => Err(error),
                        Err(rollback) => {
                            Err(error.context(format!("discard unpublished session: {rollback:#}")))
                        }
                    };
                }
                provision?;
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
        self.reload_controller().await?;
        Ok(registered)
    }

    pub async fn wait_create_session(&self, session_id: &str) -> Result<()> {
        let result = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let active = lifecycle
                .get(session_id)
                .with_context(|| format!("no create operation exists for session {session_id}"))?;
            ensure!(
                active.kind == LifecycleKind::Create,
                "session {session_id} is no longer being created"
            );
            active.result.clone()
        };
        let channel = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        match outcome? {
            DaemonLifecycleResult::Done => Ok(()),
            DaemonLifecycleResult::Move(_) => unreachable!("cleanup cannot return a move outcome"),
            DaemonLifecycleResult::DeferredCleanup => {
                unreachable!("session creation cannot schedule target cleanup")
            }
        }
    }

    pub fn request_close(&self, session_id: &str) {
        self.close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session_id.to_owned());
        self.publish_revision();
    }

    pub fn clear_close_request(&self, session_id: &str) {
        self.close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(session_id);
        self.publish_revision();
    }

    pub fn close_is_requested(&self, session_id: &str) -> bool {
        self.close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(session_id)
    }

    pub async fn close_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(active_child_session_ids(&controller.state, &session_id))
            }
        })
        .await?;
        for child_id in children {
            self.request_close(&child_id);
            let result = self.close_requested_session(child_id.clone()).await;
            self.clear_close_request(&child_id);
            result.with_context(|| format!("stop sub-agent {child_id} before its parent"))?;
        }
        self.request_close(&session_id);
        let result = self.close_requested_session(session_id.clone()).await;
        self.clear_close_request(&session_id);
        result
    }

    async fn wait_before_close(self: &Arc<Self>, session_id: &str) -> Result<()> {
        // Cancellation is a request: the old owner must actually finish before
        // close acquires the target, including an irreversible create commit.
        let pending = {
            let operations = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            operations
                .get(session_id)
                .filter(|operation| {
                    !matches!(
                        operation.kind,
                        LifecycleKind::Close | LifecycleKind::Cleanup
                    )
                })
                .map(|operation| {
                    operation.request_cancel();
                    operation.result.clone()
                })
        };
        if let Some(pending) = pending {
            if let Err(error) = Self::wait_lifecycle_result(pending.clone()).await {
                tracing::debug!(%session_id, %error, "previous lifecycle ended before close");
            }
            self.remove_completed_lifecycle(&pending);
        }

        Ok(())
    }

    async fn close_requested_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        self.wait_before_close(&session_id).await?;
        let route = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(close_route(controller.state.sessions.get(&session_id)))
            }
        })
        .await?;
        match route {
            CloseRoute::Done => return Ok(()),
            CloseRoute::DeferredCleanup => {
                self.start_deferred_cleanup(session_id)?;
                return Ok(());
            }
            CloseRoute::Graceful | CloseRoute::RecoverInterrupted => {}
        }
        let operation_session_id = session_id.clone();
        let result = self
            .run_lifecycle(
                operation_session_id,
                LifecycleKind::Close,
                move |state, session_id, cancelled| async move {
                    let _recovery_reservation = tokio::task::spawn_blocking({
                        let observer = state.recovery_observer.clone();
                        let session_id = session_id.clone();
                        let cancelled = cancelled.clone();
                        move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                    })
                    .await
                    .context("reserve recovery for daemon close task")??;
                    let mut controller = tokio::task::spawn_blocking(Controller::load)
                        .await
                        .context("load controller for daemon close task")??;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state.clone(),
                        session_id.clone(),
                    );
                    let deferred = if route == CloseRoute::RecoverInterrupted {
                        controller
                            .recover_interrupted_close_managed(
                                &session_id,
                                &executor,
                                &state.session_manager,
                            )
                            .await?
                    } else {
                        controller
                            .close_session_managed_controlled(
                                &session_id,
                                &executor,
                                &state.session_manager,
                            )
                            .await?
                    };
                    Ok(if deferred {
                        DaemonLifecycleResult::DeferredCleanup
                    } else {
                        DaemonLifecycleResult::Done
                    })
                },
            )
            .await?;
        let _ = result; // Deferred cleanup is handed off by the daemon-owned supervisor.
        Ok(())
    }

    fn start_deferred_cleanup(
        self: &Arc<Self>,
        session_id: String,
    ) -> Result<
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
    > {
        let result = self.start_or_join_lifecycle(
            session_id.clone(),
            LifecycleKind::Cleanup,
            |state, session_id, cancelled| async move {
                blocking(move || {
                    let mut controller = Controller::load()?;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state,
                        session_id.clone(),
                    );
                    controller.cleanup_stopped_target(&session_id, &executor)?;
                    Ok(DaemonLifecycleResult::Done)
                })
                .await
            },
        )?;
        let caller_result = result.clone();
        let channel = result.clone();
        let state = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = Self::wait_lifecycle_result(result).await {
                tracing::warn!(%session_id, error = format!("{error:#}"), "deferred Podman cleanup failed");
                state.push_notice(
                    &session_id,
                    "Container storage cleanup failed; the stopped session retains its target for retry.",
                );
            }
            state.remove_completed_lifecycle(&channel);
        });
        Ok(caller_result)
    }

    fn resume_retained_cleanups(self: &Arc<Self>) {
        let session_ids = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.state == SessionState::Stopped && session.target.is_some()
            })
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in session_ids {
            if crate::controller::move_session::move_owns_session(&session_id) {
                continue;
            }
            if let Err(error) = self.start_deferred_cleanup(session_id.clone()) {
                tracing::warn!(%session_id, error = format!("{error:#}"), "could not resume deferred Podman cleanup");
                self.push_notice(
                    &session_id,
                    format!("Could not resume container storage cleanup: {error:#}"),
                );
            }
        }
    }

    async fn wait_for_deferred_cleanup(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let existing = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            lifecycle.get(session_id).and_then(|active| {
                (active.kind == LifecycleKind::Cleanup).then(|| active.result.clone())
            })
        };
        let result = match existing {
            Some(result) => result,
            None => {
                let needs_cleanup = blocking({
                    let session_id = session_id.to_owned();
                    move || {
                        let controller = Controller::load()?;
                        Ok(controller
                            .state
                            .sessions
                            .get(&session_id)
                            .is_some_and(|session| {
                                session.state == SessionState::Stopped && session.target.is_some()
                            }))
                    }
                })
                .await?;
                if !needs_cleanup {
                    return Ok(());
                }
                self.start_deferred_cleanup(session_id.to_owned())?
            }
        };
        let channel = result.clone();
        let outcome = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        match outcome? {
            DaemonLifecycleResult::Done => Ok(()),
            DaemonLifecycleResult::Move(_) => unreachable!("cleanup cannot return a move outcome"),
            DaemonLifecycleResult::DeferredCleanup => {
                unreachable!("cleanup cannot schedule another cleanup")
            }
        }
    }

    /// Resume a session, and return nothing.
    ///
    /// This used to answer with the whole `MaterializedSession`. That reply
    /// travels as one JSON frame against `MAX_FRAME_BYTES`, so a session whose
    /// projection outgrew 8 MiB could not be resumed at all — it built a
    /// several-hundred-megabyte buffer and then refused to send it. The
    /// projection is already durable; a viewer reads it from the store.
    pub async fn resume_session(self: &Arc<Self>, request: ResumeSessionRequest) -> Result<()> {
        let session_id = request.session_id.clone();
        self.wait_for_deferred_cleanup(&session_id).await?;
        // Whether it is already running is a boolean. Answering it used to
        // load the entire projection so it could be handed back as the reply.
        let already_running = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(controller
                    .state
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.state == SessionState::Running))
            }
        })
        .await?;
        if already_running {
            return Ok(());
        }
        let profile_id = request.profile_id.clone();
        let target_template_id = request.target_template_id.clone();
        let workspace_id = request.workspace_id.clone();
        let rebind_workspace_id = workspace_id.clone();
        let operation_session_id = session_id.clone();
        let result = self.start_or_join_lifecycle_for_workspace(
            session_id,
            LifecycleKind::Resume,
            Some(workspace_id),
            move |state, session_id, cancelled| async move {
                let _recovery_reservation = tokio::task::spawn_blocking({
                    let observer = state.recovery_observer.clone();
                    let session_id = session_id.clone();
                    let cancelled = cancelled.clone();
                    move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                })
                .await
                .context("reserve recovery for daemon resume task")??;
                blocking({
                    let session_id = session_id.clone();
                    move || {
                        crate::database::reassign_resumable_session_workspace(
                            &session_id,
                            &rebind_workspace_id,
                        )
                    }
                })
                .await?;
                let restore_request = request.clone();
                let mut controller = tokio::task::spawn_blocking(move || {
                    session_move::load_controller_for_resume(&restore_request)
                })
                .await
                .context("load controller for daemon resume task")??;
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state.clone(),
                    session_id.clone(),
                );
                let materialized = controller
                    .resume_session_controlled_with_repository_preflight(
                        &session_id,
                        &request.profile_id,
                        &request.target_template_id,
                        SessionResumeOptions {
                            additional_mounts: request.additional_mounts,
                            resource_allocation: request.resource_allocation,
                            discard_queue: request.discard_queue,
                        },
                        request.repository_preflight,
                        &executor,
                    )
                    .await?;
                // The projection stays where it was written. A viewer reads
                // it from the store; shipping it back through the daemon
                // reply put a whole transcript in one IPC frame.
                let _ = materialized;
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
        self.set_lifecycle_resume_destination(
            &operation_session_id,
            profile_id,
            target_template_id,
        );
        let channel = result.clone();
        let result = Self::wait_lifecycle_result(result).await;
        self.remove_completed_lifecycle(&channel);
        match result? {
            DaemonLifecycleResult::Done => {}
            DaemonLifecycleResult::Move(_) => unreachable!("resume cannot return a move outcome"),
            DaemonLifecycleResult::DeferredCleanup => {
                unreachable!("session resume cannot schedule target cleanup")
            }
        }
        blocking(move || {
            if let Some(mut operation) =
                crate::database::load_move_operation(&operation_session_id)?
                && !operation.queue_admission_started
            {
                operation.phase = mj_core::state::MovePhase::Cancelled;
                operation.queue_admission_finished = true;
                operation.updated_at = chrono::Utc::now().to_rfc3339();
                operation.error = Some("Recovered through an explicit Resume operation".into());
                crate::database::save_move_operation(&operation)?;
            }
            Ok(())
        })
        .await?;
        Ok(())
    }

    async fn force_stop_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(active_child_session_ids(&controller.state, &session_id))
            }
        })
        .await?;
        for child_id in children {
            Box::pin(self.force_stop_session(child_id.clone()))
                .await
                .with_context(|| format!("force-stop sub-agent {child_id} before its parent"))?;
        }
        let operation_session_id = session_id.clone();
        let result = self
            .run_lifecycle(
                operation_session_id,
                LifecycleKind::ForceStop,
                |state, session_id, cancelled| async move {
                    blocking(move || {
                        let mut controller = Controller::load()?;
                        let executor = DaemonStageReportingExecutor::new(
                            CancellableProcessExecutor::new(cancelled),
                            state,
                            session_id.clone(),
                        );
                        let deferred = controller.force_stop(&session_id, &executor)?;
                        Ok(if deferred {
                            DaemonLifecycleResult::DeferredCleanup
                        } else {
                            DaemonLifecycleResult::Done
                        })
                    })
                    .await
                },
            )
            .await?;
        let _ = result; // The lifecycle supervisor owns the cleanup handoff.
        Ok(())
    }

    async fn destroy_stopped_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                Ok(crate::database::list_subagents(&session_id)?
                    .into_iter()
                    .map(|child| child.child_session_id)
                    .collect::<Vec<_>>())
            }
        })
        .await?;
        for child_id in children {
            Box::pin(self.force_destroy_session(child_id.clone()))
                .await
                .with_context(|| format!("destroy sub-agent {child_id} before its parent"))?;
        }
        self.wait_for_deferred_cleanup(&session_id).await?;
        let exists = blocking({
            let session_id = session_id.clone();
            move || Ok(Controller::load()?.state.sessions.contains_key(&session_id))
        })
        .await?;
        if !exists {
            return Ok(());
        }
        self.run_lifecycle(
            session_id,
            LifecycleKind::DestroyStopped,
            |state, session_id, cancelled| async move {
                blocking(move || {
                    let mut controller = Controller::load()?;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state,
                        session_id.clone(),
                    );
                    controller.destroy_session_controlled(&session_id, &executor)?;
                    Ok(DaemonLifecycleResult::Done)
                })
                .await
            },
        )
        .await?;
        Ok(())
    }

    /// Cancel any in-flight lifecycle for `session_id` and wait for it to
    /// finish.
    ///
    /// Force destruction is the escape hatch for a wedged operation, so it
    /// takes over rather than queueing behind one — but only after the running
    /// task has stopped, because a cancelled create or close re-persists its
    /// record as it unwinds and would otherwise resurrect the row this
    /// operation deletes. A lifecycle that ignores cancellation for longer
    /// than [`FORCE_DESTROY_PREEMPT_TIMEOUT`] is reported instead of destroyed
    /// under.
    async fn preempt_active_lifecycle(self: &Arc<Self>, session_id: &str) -> Result<()> {
        let mut result = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(active) = lifecycle.get(session_id) else {
                return Ok(());
            };
            if !active.result.borrow().is_none() {
                return Ok(());
            }
            active.request_cancel();
            active.result.clone()
        };
        let finished = tokio::time::timeout(FORCE_DESTROY_PREEMPT_TIMEOUT, async {
            loop {
                if result.borrow().is_some() {
                    return Ok(());
                }
                if result.changed().await.is_err() {
                    return Err(());
                }
            }
        })
        .await;
        match finished {
            // The loop only returns once the watch holds a result or its
            // sender died; distinguish those two, and the timeout separately.
            Ok(Ok(())) => Ok(()),
            Ok(Err(())) => bail!(
                "daemon lifecycle operation stopped without a result for session {session_id}"
            ),
            Err(_) => bail!(
                "session {session_id} still has an operation that did not stop after cancellation; try again"
            ),
        }
    }

    /// Permanently destroy a session from any state, cancelling whatever
    /// lifecycle operation holds it first. Data loss is the caller's confirmed
    /// decision; see [`Controller::force_destroy_session`].
    pub async fn force_destroy_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let children = blocking({
            let session_id = session_id.clone();
            move || {
                Ok(crate::database::list_subagents(&session_id)?
                    .into_iter()
                    .map(|child| child.child_session_id)
                    .collect::<Vec<_>>())
            }
        })
        .await?;
        for child_id in children {
            Box::pin(self.force_destroy_session(child_id.clone()))
                .await
                .with_context(|| format!("destroy sub-agent {child_id} before its parent"))?;
        }
        self.preempt_active_lifecycle(&session_id).await?;
        let exists = blocking({
            let session_id = session_id.clone();
            move || Ok(Controller::load()?.state.sessions.contains_key(&session_id))
        })
        .await?;
        if !exists {
            return Ok(());
        }
        self.run_lifecycle(
            session_id,
            LifecycleKind::ForceDestroy,
            |state, session_id, cancelled| async move {
                let _recovery_reservation = tokio::task::spawn_blocking({
                    let observer = state.recovery_observer.clone();
                    let session_id = session_id.clone();
                    let cancelled = cancelled.clone();
                    move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                })
                .await
                .context("reserve recovery for daemon force-destroy task")??;
                blocking({
                    let session_id = session_id.clone();
                    move || {
                        let mut controller = Controller::load()?;
                        let executor = DaemonStageReportingExecutor::new(
                            CancellableProcessExecutor::new(cancelled),
                            state,
                            session_id.clone(),
                        );
                        controller.force_destroy_session(&session_id, &executor)?;
                        crate::controller::move_session::release_move_queue_hold(&session_id);
                        Ok(DaemonLifecycleResult::Done)
                    }
                })
                .await
            },
        )
        .await?;
        Ok(())
    }

    /// Force-delete a workspace: destroy every active session in it (see
    /// [`RuntimeState::force_destroy_session`]), drop its detached drafts, and
    /// remove the workspace row. Stopped histories stay globally resumable.
    ///
    /// In-flight resumes into the workspace still refuse the deletion because
    /// they have not yet claimed a durable session workspace. A session that
    /// fails to destroy stops the sequence with the remainder named, so the
    /// operation can be retried without losing progress.
    pub async fn force_delete_workspace(self: &Arc<Self>, workspace_id: String) -> Result<()> {
        ensure!(
            !self.workspace_has_active_resume(&workspace_id),
            "workspace has a session resume in progress"
        );
        let sessions = blocking({
            let workspace_id = workspace_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(active_sessions_for_force_destruction(
                    &controller,
                    &workspace_id,
                ))
            }
        })
        .await?;
        for (index, session_id) in sessions.iter().enumerate() {
            if let Err(error) = self.force_destroy_session(session_id.clone()).await {
                let remaining = sessions.len() - index - 1;
                bail!(
                    "force-destroying session {session_id} failed: {error:#}; \
                     {remaining} session(s) in the workspace remain"
                );
            }
        }
        blocking({
            let workspace_id = workspace_id.clone();
            move || crate::database::force_delete_workspace(&workspace_id)
        })
        .await?;
        refresh_runtime_workspaces(self).await?;
        Ok(())
    }

    fn cancel_lifecycle(&self, session_id: &str) -> Result<()> {
        // Controller before lifecycle, the order worker polling takes.
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let active = lifecycle.get(session_id).with_context(|| {
            format!("no lifecycle operation is running for session {session_id}")
        })?;
        ensure!(
            lifecycle_cancellable(active.kind, durable_session_state(&controller, session_id)),
            "stop of {session_id} has passed its verified checkpoint and is removing the target; \
             it cannot be cancelled"
        );
        ensure!(
            active.request_cancel(),
            "lifecycle operation is no longer cancellable"
        );
        drop(lifecycle);
        drop(controller);
        self.publish_revision();
        Ok(())
    }

    /// Let storage cleanup drain briefly, then cancel and join every lifecycle
    /// owner before the daemon closes its session manager and database writer.
    /// One shared deadline bounds all cleanup tasks rather than granting eight
    /// seconds to each session serially.
    async fn cancel_and_wait_lifecycles(&self) -> Result<()> {
        let mut pending = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            lifecycle
                .iter()
                .filter(|(_, active)| active.result.borrow().is_none())
                .map(|(session_id, active)| {
                    if active.kind != LifecycleKind::Cleanup {
                        active.request_cancel();
                    }
                    let stage = active
                        .active_stages
                        .keys()
                        .next_back()
                        .map(|stage| stage.label())
                        .unwrap_or_else(|| "container cleanup".to_owned());
                    (
                        session_id.clone(),
                        active.kind,
                        stage,
                        active.cancelled.clone(),
                        active.result.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        for (session_id, kind, stage, cancelled, result) in &mut pending {
            if *kind != LifecycleKind::Cleanup || result.borrow().is_some() {
                continue;
            }
            tracing::info!(%session_id, %stage, "daemon shutdown is waiting for deferred cleanup");
            self.set_lifecycle_notice(
                session_id,
                &format!("Daemon shutdown is waiting for {stage}"),
            );
            let finished = tokio::time::timeout_at(cleanup_deadline, async {
                while result.borrow_and_update().is_none() {
                    result.changed().await.with_context(|| {
                        format!("cleanup owner stopped without a result for session {session_id}")
                    })?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
            match finished {
                Ok(result) => result?,
                Err(_) => {
                    tracing::warn!(%session_id, %stage, "deferred cleanup exceeded the daemon shutdown drain deadline");
                    cancelled.store(true, Ordering::Release);
                }
            }
        }
        let join_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        for (session_id, _, stage, cancelled, mut result) in pending {
            cancelled.store(true, Ordering::Release);
            let joined = tokio::time::timeout_at(join_deadline, async {
                while result.borrow_and_update().is_none() {
                    result.changed().await.with_context(|| {
                        format!("lifecycle owner stopped without a result for session {session_id}")
                    })?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await;
            if joined.is_err() {
                bail!(
                    "timed out cancelling lifecycle owner for session {session_id} while {stage}"
                );
            }
            joined.expect("checked timeout")?;
        }
        Ok(())
    }

    /// Every lifecycle operation running now.
    ///
    /// The dashboard receives these through a watch channel built by its own
    /// poller, which the phone server does not have; rather than plumb that
    /// channel through the session-manager handle, the phone loop reads the
    /// same state directly. The read is a mutex acquisition over a small map,
    /// and it happens once per published snapshot, so it never blocks the
    /// loop the way an await on the async snapshot path would.
    pub fn active_lifecycles(&self) -> Vec<RuntimeLifecycleView> {
        // Controller before lifecycle, the order worker polling takes. Both are
        // plain mutex acquisitions over small maps, so a render loop calling
        // this never awaits.
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.active_lifecycles_with(&controller)
    }

    /// The same view for a caller that already holds the controller lock.
    /// The lock is not reentrant, so taking it again here would deadlock the
    /// daemon.
    fn active_lifecycles_with(&self, controller: &Controller) -> Vec<RuntimeLifecycleView> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, active)| active.is_visible())
            .map(|(session_id, active)| RuntimeLifecycleView {
                operation_id: active.operation_id.clone(),
                cancellable: active.is_cancellable()
                    && lifecycle_cancellable(
                        active.kind,
                        durable_session_state(controller, session_id),
                    ),
                session_id: session_id.clone(),
                kind: active.kind.into(),
                started_at_epoch_seconds: active.started_at_epoch_seconds,
                active_stages: active
                    .active_stages
                    .iter()
                    .map(|(stage, (_, started_at))| (*stage, *started_at))
                    .collect(),
                resume_destination: active.resume_destination.clone(),
                notice: active.notice.clone(),
            })
            .collect()
    }

    /// The lifecycle state of one in-memory record, or `None` when the daemon
    /// holds no record for it. Reading one field costs one lock rather than a
    /// clone of every record, which is what a poll wants.
    pub fn session_state(&self, session_id: &str) -> Option<mj_core::state::SessionState> {
        if self.close_is_requested(session_id) {
            return Some(SessionState::Closing);
        }
        self.controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .get(session_id)
            .map(|record| record.state)
    }

    /// One in-memory session record, or `None` when the daemon holds none.
    pub fn session_record(&self, session_id: &str) -> Option<SessionRecord> {
        self.controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .state
            .sessions
            .get(session_id)
            .cloned()
    }

    pub async fn workspace_session_handle(
        &self,
        session_id: &str,
    ) -> Result<crate::session_manager::ManagedSessionHandle> {
        let record = self.session_record(session_id).context("unknown session")?;
        ensure!(
            record.target.is_some()
                && record.state == SessionState::Running
                && !self.close_is_requested(session_id),
            "session must have a live running target for file injection"
        );
        self.session_manager.session(session_id.to_owned()).await
    }

    /// Checkpoint a session now and publish the result, the way the daemon's
    /// own checkpoint action does.
    ///
    /// The API's bundle export needs a fresh archive for a running session. Only
    /// that session's own lifecycle operation can conflict with its checkpoint,
    /// so this refuses when the session itself is mid-operation and returns a
    /// [`SessionLifecycleBusy`] the export path can fall back on. It must not
    /// take the process-wide lifecycle guard: that rejected every export while
    /// any unrelated session anywhere was mid-lifecycle (#1010).
    pub async fn checkpoint_session_now(
        &self,
        session_id: &str,
    ) -> Result<mj_core::state::CheckpointMetadata> {
        if self.session_lifecycle_active(session_id) {
            return Err(anyhow::Error::new(SessionLifecycleBusy {
                session_id: session_id.to_owned(),
            }));
        }
        let mut controller = blocking(Controller::load).await?;
        let checkpoint = controller.checkpoint_session(session_id).await?;
        refresh_runtime_controller(self).await;
        Ok(checkpoint)
    }

    /// Whether this specific session has a lifecycle operation still running.
    /// A checkpoint conflicts only with its own session's operations, never
    /// with another session's (#1010).
    fn session_lifecycle_active(&self, session_id: &str) -> bool {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .is_some_and(|active| active.result.borrow().is_none())
    }

    /// In-memory records and ownership sampled with the same lock order as
    /// completion. A web publish must not pair old records with a new absence
    /// of ownership, even while its background database reload is in flight.
    pub fn session_projection(
        &self,
    ) -> (BTreeMap<String, SessionRecord>, Vec<RuntimeLifecycleView>) {
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let operations = self.active_lifecycles_with(&controller);
        let mut records = controller.state.sessions.clone();
        for id in self
            .close_requested
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            if let Some(record) = records.get_mut(id)
                && record.state != SessionState::Stopped
            {
                record.state = SessionState::Closing;
            }
        }
        (records, operations)
    }

    pub fn cancel_lifecycle_if_active(&self, session_id: &str) {
        if let Some(active) = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
        {
            active.request_cancel();
            self.publish_revision();
        }
    }

    fn set_lifecycle_resume_destination(
        &self,
        session_id: &str,
        profile_id: String,
        target_id: String,
    ) {
        if let Some(active) = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(session_id)
        {
            active.resume_destination = Some((profile_id, target_id));
            self.publish_revision();
        }
    }

    fn change_lifecycle_stage(&self, session_id: &str, stage: ProvisionStage, active: bool) {
        let changed = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(operation) = lifecycle.get_mut(session_id) else {
                return;
            };
            if active {
                let entry = operation
                    .active_stages
                    .entry(stage)
                    .or_insert_with(|| (0, epoch_seconds()));
                entry.0 += 1;
                entry.0 == 1
            } else {
                let Some((count, _)) = operation.active_stages.get_mut(&stage) else {
                    return;
                };
                *count -= 1;
                if *count == 0 {
                    operation.active_stages.remove(&stage);
                    true
                } else {
                    false
                }
            }
        };
        if changed {
            self.publish_revision();
        }
    }

    /// Record something the daemon did on its own, for every attached surface
    /// to report once.
    fn push_notice(&self, session_id: &str, text: impl Into<String>) {
        const RETAINED_NOTICES: usize = 32;

        let notice = RuntimeNotice {
            id: self.next_notice_id.fetch_add(1, Ordering::AcqRel),
            session_id: session_id.to_owned(),
            text: text.into(),
        };
        {
            let mut notices = self.notices.lock().unwrap_or_else(PoisonError::into_inner);
            notices.push_back(notice);
            while notices.len() > RETAINED_NOTICES {
                notices.pop_front();
            }
        }
        self.publish_revision();
    }

    fn set_lifecycle_notice(&self, session_id: &str, notice: &str) {
        if let Some(active) = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(session_id)
        {
            if active.kind == LifecycleKind::Move && notice == "Preparing destination" {
                active.move_source_closed = true;
            }
            active.notice = Some(notice.to_owned());
            self.publish_revision();
        }
    }
}

/// Log one finished worker upgrade, and tell the surfaces about the one that
/// changed something.
fn report_worker_upgrade(
    state: &RuntimeState,
    result: &crate::worker_upgrade::WorkerUpgradeResult,
) {
    use crate::controller::WorkerUpgradeOutcome;

    let session_id = &result.session_id;
    if result.cancelled {
        tracing::debug!(%session_id, "worker upgrade was preempted");
        return;
    }
    match &result.outcome {
        Ok(WorkerUpgradeOutcome::Upgraded { build }) => {
            tracing::info!(%session_id, %build, "replaced the session worker with the current build");
            let name = state
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .state
                .sessions
                .get(session_id)
                .map_or_else(
                    || session_id.clone(),
                    |session| session.display_title().to_owned(),
                );
            state.push_notice(session_id, format!("Upgraded the worker for {name}."));
        }
        Ok(WorkerUpgradeOutcome::AlreadyCurrent { build }) => {
            tracing::debug!(%session_id, %build, "session worker already runs the current build");
        }
        Ok(WorkerUpgradeOutcome::Deferred) => {
            tracing::debug!(%session_id, "worker upgrade deferred: the session is working");
        }
        Err(error) => {
            tracing::warn!(%session_id, %error, "could not upgrade the session worker");
        }
    }
}

/// Children of `parent_session_id` whose session is still active, in the
/// order they should be stopped before the parent. A child that already
/// stopped needs nothing and would make `force_stop` fail on it.
fn active_child_session_ids(state: &mj_core::state::State, parent_session_id: &str) -> Vec<String> {
    state
        .subagents
        .values()
        .filter(|child| child.parent_session_id == parent_session_id)
        .filter(|child| {
            state
                .sessions
                .get(&child.child_session_id)
                .is_some_and(|session| session.state.is_active())
        })
        .map(|child| child.child_session_id.clone())
        .collect()
}

fn runtime_records_for_workspace(
    controller: &Controller,
    session_ids: &BTreeSet<String>,
) -> Vec<SessionRecord> {
    controller
        .state
        .sessions
        .iter()
        .filter(|(session_id, session)| {
            !session.state.is_active() || session_ids.contains(*session_id)
        })
        .map(|(_, session)| session.clone())
        .collect()
}

/// Relations for the children carried in `records`, so a surface can keep a
/// daemon-created child out of the real workspace without a full state
/// reload. Filtering by the returned records, rather than by `session_ids`
/// directly, keeps this in step with `runtime_records_for_workspace`, which
/// also includes inactive sessions outside that set.
fn runtime_subagents_for_workspace(
    controller: &Controller,
    records: &[SessionRecord],
) -> Vec<SubagentRecord> {
    let record_ids: BTreeSet<&str> = records.iter().map(|record| record.id.as_str()).collect();
    controller
        .state
        .subagents
        .iter()
        .filter(|(child_session_id, _)| record_ids.contains(child_session_id.as_str()))
        .map(|(_, subagent)| subagent.clone())
        .collect()
}

struct DaemonStageReportingExecutor<E> {
    inner: E,
    state: Arc<RuntimeState>,
    session_id: String,
}

impl<E> DaemonStageReportingExecutor<E> {
    fn new(inner: E, state: Arc<RuntimeState>, session_id: String) -> Self {
        Self {
            inner,
            state,
            session_id,
        }
    }
}

impl<E: CommandExecutor> CommandExecutor for DaemonStageReportingExecutor<E> {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        let _stage = command
            .stage
            .map(|stage| ProvisionStageGuard::new(self, stage));
        let started = std::time::Instant::now();
        let result = self.inner.execute(command);
        tracing::info!(
            session_id = %self.session_id,
            stage = command
                .stage
                .map(ProvisionStage::label)
                .unwrap_or_else(|| "command".to_owned()),
            purpose = %command.purpose,
            duration_ms = started.elapsed().as_millis(),
            succeeded = result.as_ref().is_ok_and(|output| output.status == 0),
            "session command stage finished"
        );
        result
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        let _stage = command
            .stage
            .map(|stage| ProvisionStageGuard::new(self, stage));
        let started = std::time::Instant::now();
        let result = self.inner.execute_with_stdin(command, input);
        tracing::info!(
            session_id = %self.session_id,
            stage = command
                .stage
                .map(ProvisionStage::label)
                .unwrap_or_else(|| "command".to_owned()),
            purpose = %command.purpose,
            duration_ms = started.elapsed().as_millis(),
            succeeded = result.as_ref().is_ok_and(|output| output.status == 0),
            "session streaming command stage finished"
        );
        result
    }

    fn cancellation_requested(&self) -> bool {
        self.inner.cancellation_requested()
    }

    fn stage_started(&self, stage: ProvisionStage) {
        self.state
            .change_lifecycle_stage(&self.session_id, stage, true);
    }

    fn stage_finished(&self, stage: ProvisionStage) {
        self.state
            .change_lifecycle_stage(&self.session_id, stage, false);
    }

    fn notify_notice(&self, notice: &str) {
        self.state.set_lifecycle_notice(&self.session_id, notice);
    }
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_hex<const N: usize>() -> Result<String> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("generate daemon secret: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_metadata(path: &Path, metadata: &DaemonMetadata) -> Result<()> {
    let parent = path
        .parent()
        .context("daemon metadata path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create daemon data directory {}", parent.display()))?;
    let temporary = parent.join(format!(".daemon.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(metadata)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(&body)?;
    file.sync_all()?;
    fs::rename(&temporary, path)
        .with_context(|| format!("publish daemon metadata {}", path.display()))?;
    Ok(())
}

/// PID of the process this daemon must not outlive, if one was requested.
///
/// Tests start daemons that no client ever attaches to, so idle exit cannot
/// retire them, and a test process that dies without unwinding never runs its
/// teardown. Naming an owner makes the daemon responsible for its own lifetime.
fn owner_pid_to_watch() -> Result<Option<u32>> {
    let Some(value) = mj_core::config::env_override("DAEMON_OWNER_PID") else {
        return Ok(None);
    };
    let pid: u32 = value
        .trim()
        .parse()
        .map_err(|_| anyhow!("MJ_DAEMON_OWNER_PID must be a process id, but it is {value:?}"))?;
    ensure!(
        process_is_alive(pid),
        "MJ_DAEMON_OWNER_PID names process {pid}, which is not running"
    );
    Ok(Some(pid))
}

pub async fn run_daemon_process() -> Result<()> {
    // Checked before the store is locked so a bad value fails fast and leaves
    // no daemon state behind.
    let owner_pid = owner_pid_to_watch()?;
    let guard = ControllerStoreGuard::acquire()?;
    let database_writer = guard.start_database_writer()?;
    let epilogue_started = AtomicBool::new(false);
    let mut outcome = run_daemon_runtime(&epilogue_started, owner_pid).await;
    if !epilogue_started.load(Ordering::Acquire) {
        // Initialization failed before the runtime-owned epilogue existed.
        // The same process-level bound still applies to closing the writer.
        spawn_shutdown_watchdog();
    }
    let writer_shutdown = tokio::task::spawn_blocking(move || database_writer.shutdown())
        .await
        .context("database writer shutdown task panicked")
        .and_then(std::convert::identity);
    record_daemon_cleanup(&mut outcome, "shut down database writer", writer_shutdown);
    outcome
}

async fn run_daemon_runtime(epilogue_started: &AtomicBool, owner_pid: Option<u32>) -> Result<()> {
    // Freeze worker sources before any session can be created or upgraded.
    // Copying binaries belongs on a blocking task, never the runtime event loop.
    tokio::task::spawn_blocking(crate::controller::pin_worker_binary_sources)
        .await
        .context("worker source snapshot task failed")??;
    Controller::recover_config_id_rename()?;
    let config = Config::load()?;
    crate::database::recover_interrupted_checkpointing_sessions(
        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )?;
    crate::controller::reconcile_managed_checkpoint_archives()?;

    let controller = Controller::load()?;
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .context("bind Mjolnir daemon loopback endpoint")?;
    let metadata = DaemonMetadata {
        protocol_version: PROTOCOL_VERSION,
        pid: std::process::id(),
        address: listener.local_addr()?,
        token: random_hex::<32>()?,
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        build_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let workspaces = tokio::task::spawn_blocking(crate::database::list_workspaces)
        .await
        .context("daemon workspace load task panicked")??;
    let mut remote = if config.phone.enabled {
        Some(spawn_remote_session_manager()?)
    } else {
        None
    };

    // Start the primary manager last: every remaining fallible operation is
    // inside `outcome`, so its owner always reaches the awaited epilogue.
    let manager = spawn_session_manager()?;
    let manager_targets = manager.targets;
    manager_targets.send_replace(dashboard_worker_targets(&controller));
    let mut manager_updates = manager.updates;
    let manager_control = manager.control.clone();
    let manager_shutdown = manager.shutdown;
    let mut recovery = crate::recovery::RecoveryCoordinator::spawn(manager_control.clone());
    let recovery_observer = recovery.observer();
    // Shares the recovery gate, so a recovery copy and a worker upgrade never
    // act on one session at the same time.
    let mut worker_upgrades = crate::worker_upgrade::WorkerUpgradeCoordinator::spawn(
        manager_control.clone(),
        &recovery_observer,
    );
    let state = Arc::new(RuntimeState::new(
        manager_control.clone(),
        Controller {
            config: controller.config.clone(),
            state: controller.state.clone(),
        },
        recovery_observer.clone(),
        worker_upgrades.observer(),
        workspaces,
    ));
    let move_operations = blocking(crate::database::load_move_operations).await?;
    let move_owned = state.recover_moves(move_operations)?;
    state.resume_retained_cleanups();
    let cancellation = crate::termination::Coordinator::install().token();
    let target_refresh = spawn_manager_target_refresher(
        manager_targets.clone(),
        cancellation.clone(),
        state.clone(),
    );
    let image_refresh = spawn_image_refresher(
        {
            let state = state.clone();
            move || state.with_config(crate::controller::image_refresh_plan)
        },
        cancellation.clone(),
    );
    let exit_when_idle = mj_core::config::env_override_os("DAEMON_EXIT_WHEN_IDLE").is_some();
    let mut idle_tick = tokio::time::interval(Duration::from_millis(100));
    idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut owner_tick = tokio::time::interval(Duration::from_millis(500));
    owner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut recovery_tick = tokio::time::interval(Duration::from_millis(250));
    recovery_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (interrupted_close_tx, mut interrupted_close_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut interrupted_close_cancellations = Vec::new();
    let mut interrupted_close_tasks = Vec::new();
    for session_id in interrupted_close_session_ids(&controller) {
        if move_owned.contains(&session_id) {
            continue;
        }
        let interrupted_cancellation = Arc::new(AtomicBool::new(false));
        let interrupted_close_task = spawn_interrupted_close_recovery(
            session_id,
            manager_control.clone(),
            recovery_observer.clone(),
            interrupted_cancellation.clone(),
            interrupted_close_tx.clone(),
            None,
        );
        interrupted_close_cancellations.push(interrupted_cancellation);
        interrupted_close_tasks.push(interrupted_close_task);
    }
    let mut phone_publisher: Option<RemoteSessionPublisher> = None;
    let mut phone_task = None;
    let mut remote_request_bridge = None;
    if let Some(remote) = remote.take() {
        remote
            .targets
            .send_replace(dashboard_worker_targets(&controller));
        phone_publisher = Some(remote.publisher.clone());
        remote_request_bridge = Some(spawn_remote_request_bridge(
            remote.requests,
            manager_control.clone(),
        ));
        phone_task = Some(spawn_phone_server(
            config.phone,
            cancellation.clone(),
            state.clone(),
            SessionManagerChannels {
                targets: remote.targets,
                control: remote.control,
                updates: remote.updates,
                shutdown: remote.shutdown,
            },
        ));
    } else {
        state.set_phone_status(WebViewerStatus::Disabled);
        state.web_viewer.publish(crate::server::WebViewerAccess::Unavailable("Web access is disabled. Enable [phone].enabled in your configuration, then restart the daemon.".into()));
    }
    let daemon_metadata_path = metadata_path();
    let mut client_tasks = tokio::task::JoinSet::new();

    // Everything a client can use is initialized before this atomic
    // publication. From here on every exit, including an error from the test
    // hook or the event loop, flows through the same bounded epilogue.
    let mut outcome = async {
        write_metadata(&daemon_metadata_path, &metadata)?;
        reach_test_hook("daemon_metadata_before_listening").await?;
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = idle_tick.tick(), if exit_when_idle && state.ever_attached.load(Ordering::Acquire) => {
                    state.prune_dead_clients();
                    if state.attachments().is_empty() {
                        break;
                    }
                }
                _ = owner_tick.tick(), if owner_pid.is_some() => {
                    if let Some(owner) = owner_pid
                        && !process_is_alive(owner)
                    {
                        tracing::info!(owner_pid = owner, "daemon owner process exited; shutting down");
                        break;
                    }
                }
                _ = recovery_tick.tick() => {
                    while let Some(result) = recovery.try_result() {
                        if let Err(error) = &result.outcome {
                            // A deferred copy found the agent working. That is
                            // the normal state of a session in use, so it is
                            // news, not a fault.
                            if result.deferred {
                                tracing::info!(session_id = %result.session_id, %error, "recovery copy deferred: agent is working");
                            } else {
                                tracing::warn!(session_id = %result.session_id, %error, "daemon recovery checkpoint failed");
                            }
                        }
                        refresh_runtime_controller(&state).await;
                    }
                    while let Some(result) = worker_upgrades.try_result() {
                        report_worker_upgrade(&state, &result);
                    }
                }
                completed = interrupted_close_rx.recv() => {
                    if let Some(completed) = completed {
                        let recovered = completed.result.is_ok();
                        if let Err(error) = completed.result {
                            tracing::warn!(session_id = %completed.session_id, %error, "daemon could not resume interrupted close");
                        }
                        refresh_runtime_controller(&state).await;
                        if recovered && completed.deferred_cleanup
                            && let Err(error) = state.start_deferred_cleanup(completed.session_id.clone())
                        {
                            tracing::warn!(session_id = %completed.session_id, error = format!("{error:#}"), "could not continue cleanup after interrupted close");
                            state.push_notice(
                                &completed.session_id,
                                format!("Could not continue container storage cleanup: {error:#}"),
                            );
                        }
                    }
                }
                accepted = listener.accept() => {
                    let (stream, peer) = accepted.context("accept Mjolnir daemon client")?;
                    if !peer.ip().is_loopback() {
                        tracing::warn!(%peer, "rejected non-loopback daemon client");
                        continue;
                    }
                    let metadata = metadata.clone();
                    let state = state.clone();
                    let cancellation = cancellation.clone();
                    client_tasks.spawn(async move {
                        if let Err(error) = serve_client(stream, metadata, state, cancellation).await {
                            tracing::debug!(error = format!("{error:#}"), "daemon client disconnected");
                        }
                    });
                }
                completed = client_tasks.join_next(), if !client_tasks.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%error, "daemon client task failed");
                    }
                }
                update = manager_updates.recv() => {
                    let Some(update) = update else {
                        bail!("controller daemon session manager stopped");
                    };
                    if let Some((detail, observed_updated_at)) =
                        state.missing_target_record(&update.session_id, &update.view)
                    {
                        let state = state.clone();
                        let session_id = update.session_id.clone();
                        client_tasks.spawn(async move {
                            if let Err(error) = state.persist_missing_target(
                                &session_id, detail, observed_updated_at,
                            ).await {
                                tracing::warn!(%session_id, %error, "could not persist missing worker target");
                                state.push_notice(&session_id, format!("Could not record missing session target: {error:#}"));
                            }
                        });
                    }
                    if let Some(publisher) = phone_publisher.as_ref()
                        && let Err(error) = publisher.try_publish(
                            update.session_id.clone(),
                            update.view.clone(),
                        )
                    {
                        tracing::warn!(%error, "phone session view bridge stopped");
                        phone_publisher = None;
                    }
                    // Every session's view passes here whether or not anything is
                    // attached, which is exactly what an automatic review needs to
                    // see: the turn that just finished.
                    state.review_host().observe(&update.session_id, &update.view);
                    state.publish_session(update.session_id, update.view).await?;
                }
            }
        }
        Ok(())
    }
    .await;

    epilogue_started.store(true, Ordering::Release);
    spawn_shutdown_watchdog();
    // Idle exit and fallible loop exits do not arrive through the termination
    // coordinator. Stop every daemon-owned task before closing the sole writer.
    cancellation.cancel();
    for interrupted_cancellation in interrupted_close_cancellations {
        interrupted_cancellation.store(true, Ordering::Release);
    }
    drop(interrupted_close_tx);
    record_daemon_cleanup(
        &mut outcome,
        "remove daemon metadata",
        remove_daemon_metadata(&daemon_metadata_path),
    );
    record_daemon_cleanup(
        &mut outcome,
        "shut down turn review host",
        state
            .review_host()
            .shutdown()
            .await
            .map_err(anyhow::Error::msg),
    );
    record_daemon_cleanup(
        &mut outcome,
        "join controller target refresher",
        target_refresh.await.map_err(anyhow::Error::new),
    );
    record_daemon_cleanup(
        &mut outcome,
        "join container image refresher",
        image_refresh.await.map_err(anyhow::Error::new),
    );
    if let Some(phone_task) = phone_task {
        record_daemon_cleanup(
            &mut outcome,
            "join phone server",
            phone_task.await.map_err(anyhow::Error::new),
        );
    }
    if let Some(remote_request_bridge) = remote_request_bridge {
        record_daemon_cleanup(
            &mut outcome,
            "join phone session request bridge",
            remote_request_bridge.await.map_err(anyhow::Error::new),
        );
    }
    client_tasks.abort_all();
    while let Some(result) = client_tasks.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            record_daemon_cleanup(
                &mut outcome,
                "join daemon client task",
                Err(anyhow::Error::new(error)),
            );
        }
    }
    record_daemon_cleanup(
        &mut outcome,
        "cancel daemon lifecycle operations",
        state.cancel_and_wait_lifecycles().await,
    );
    for interrupted_close_task in interrupted_close_tasks {
        record_daemon_cleanup(
            &mut outcome,
            "join interrupted close recovery",
            interrupted_close_task.await.map_err(anyhow::Error::new),
        );
    }
    drop(recovery);
    record_daemon_cleanup(
        &mut outcome,
        "shut down controller daemon session manager",
        manager_shutdown.shutdown().await,
    );
    outcome
}

fn remove_daemon_metadata(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

/// Keep the event-loop failure as the primary result while still running and
/// reporting every cleanup step. If the loop ended normally, the first
/// cleanup failure becomes the daemon's result.
fn record_daemon_cleanup(outcome: &mut Result<()>, operation: &'static str, cleanup: Result<()>) {
    let Err(error) = cleanup else {
        return;
    };
    let error = error.context(operation);
    if outcome.is_ok() {
        *outcome = Err(error);
    } else {
        tracing::warn!(error = format!("{error:#}"), "daemon cleanup step failed");
    }
}

/// Bounds the epilogue below.
///
/// The daemon leaves on its own long before this fires: a graceful exit
/// returns from `run_daemon_process`, the process exits 0, and this task dies
/// with the runtime. It exists so no unwinding step can hold the process open
/// past the deadline its clients wait on, whatever the cause of the shutdown.
fn spawn_shutdown_watchdog() {
    tokio::spawn(async move {
        tokio::time::sleep(SHUTDOWN_FORCE_EXIT_TIMEOUT).await;
        tracing::error!(
            seconds = SHUTDOWN_FORCE_EXIT_TIMEOUT.as_secs(),
            "daemon shutdown did not finish in time; exiting"
        );
        // The metadata file points clients at a process that is about to stop
        // answering. Removing it is what the epilogue would have done.
        if let Err(error) = fs::remove_file(metadata_path())
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "could not remove daemon metadata before the forced exit");
        }
        // 128 + signal is reserved for exits that really were signalled.
        std::process::exit(1);
    });
}

fn spawn_manager_target_refresher(
    targets: tokio::sync::watch::Sender<Vec<crate::session_manager::RelaySessionTarget>>,
    cancellation: CancellationToken,
    state: Arc<RuntimeState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    // Keep a controller loaded from the old config from being
                    // installed after a concurrent id rename has committed.
                    let _config_mutation = state.config_mutation.lock().await;
                    match tokio::task::spawn_blocking(Controller::load).await {
                        Ok(Ok(controller)) => {
                            // Startup, force-stop, relocation, and the teardown
                            // phase of close own the worker target. Graceful
                            // close keeps polling only until it has released the
                            // manager lease after sealing the relay.
                            let lifecycle_sessions =
                                state.worker_poll_exclusion_session_ids(&controller);
                            let refreshed = dashboard_worker_targets_excluding(
                                &controller,
                                &lifecycle_sessions,
                            );
                            let changed = {
                                let mut review = state
                                    .review_config
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                review.clone_from(&controller.config.review);
                                drop(review);
                                let mut current = state
                                    .controller
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner);
                                let changed = current.config != controller.config;
                                *current = controller;
                                changed
                            };
                            // Prune the review host's retained transcripts to the
                            // same live set, so a stopped or destroyed session's
                            // MaterializedSession does not linger there forever.
                            state.review_host().retain_sessions(
                                refreshed
                                    .iter()
                                    .map(|target| target.session_id.clone())
                                    .collect(),
                            );
                            targets.send_replace(refreshed);
                            if changed {
                                state.publish_revision();
                            }
                        }
                        Ok(Err(error)) => {
                            // The one place divergence is classified. Every
                            // read re-checks store compatibility, so an
                            // incompatible migration reaches this branch
                            // within one tick. A daemon that
                            // cannot read its own store cannot serve anyone,
                            // and its writer is already refusing work, so the
                            // answer is the shutdown it already knows how to
                            // perform.
                            if let Some(mismatch) = error
                                .chain()
                                .find_map(|cause| cause.downcast_ref::<StoreSchemaMismatch>())
                            {
                                tracing::error!(
                                    found = mismatch.found,
                                    supported = mismatch.supported,
                                    error = %mismatch,
                                    "daemon store schema diverged underneath the daemon; shutting down"
                                );
                                cancellation.cancel();
                                return;
                            }
                            tracing::warn!(error = format!("{error:#}"), "could not refresh daemon session targets");
                        }
                        Err(error) => {
                            tracing::error!(%error, "daemon target refresh task failed");
                            return;
                        }
                    }
                }
            }
        }
    })
}

async fn refresh_runtime_controller(state: &RuntimeState) {
    if let Err(error) = state.reload_controller().await {
        tracing::warn!(
            error = format!("{error:#}"),
            "could not refresh daemon controller state"
        );
    }
}

async fn refresh_runtime_workspaces(state: &RuntimeState) -> Result<()> {
    let workspaces = tokio::task::spawn_blocking(crate::database::list_workspaces)
        .await
        .context("daemon workspace refresh task panicked")??;
    state.publish_workspaces(workspaces);
    Ok(())
}

fn spawn_phone_server(
    config: mj_core::config::PhoneConfig,
    cancellation: CancellationToken,
    state: Arc<RuntimeState>,
    worker: SessionManagerChannels,
) -> tokio::task::JoinHandle<()> {
    state.set_phone_status(WebViewerStatus::Starting);
    let workspaces = state.workspaces();
    tokio::spawn(async move {
        match crate::server_runtime::run_server(
            (&config).into(),
            cancellation.clone(),
            worker,
            state.clone(),
            workspaces,
        )
        .await
        {
            Ok(()) if cancellation.is_cancelled() => {}
            Ok(()) => {
                state.set_phone_status(WebViewerStatus::Stopped);
                state.web_viewer.publish(crate::server::WebViewerAccess::Unavailable("The web viewer stopped unexpectedly. Restart the daemon to restore web access.".into()));
            }
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "phone server stopped");
                state.publish_web_access(crate::server::WebViewerAccess::Unavailable(format!(
                    "Could not start the web viewer: {error:#}"
                )));
            }
        }
    })
}

fn spawn_remote_request_bridge(
    mut requests: crate::session_manager::RemoteSessionRequests,
    manager: SessionManagerControl,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // One session's requests reach its relay actor in the order they were
        // made; different sessions still overlap.
        let mut request_order = crate::session_manager::SessionRequestOrder::new();
        while let Some(request) = requests.recv().await {
            let manager = manager.clone();
            request_order.dispatch(request, move |request| {
                forward_in_process_session_request(request, manager)
            });
        }
    })
}

async fn forward_in_process_session_request(
    request: RemoteSessionRequest,
    manager: SessionManagerControl,
) {
    match request {
        RemoteSessionRequest::Submit {
            session_id,
            command_id,
            command,
            admission,
            reply,
        } => {
            if admission.is_some() {
                let _ = reply.send(Err(
                    "review delivery admissions cannot cross the daemon request bridge".into(),
                ));
                return;
            }
            let result = async {
                manager
                    .wait_for_session(&session_id, Duration::from_secs(5))
                    .await?
                    .submit(command_id, command)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Sync { session_id, reply } => {
            let result = async { manager.session(session_id).await?.sync_now().await }
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::RespondElicitation {
            session_id,
            elicitation_id,
            response,
            reply,
        } => {
            let result = async {
                manager
                    .session(session_id)
                    .await?
                    .respond_elicitation(elicitation_id, response)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::StopBackgroundTask {
            session_id,
            background_task_id,
            reply,
        } => {
            let result = async {
                manager
                    .session(session_id)
                    .await?
                    .stop_background_task(background_task_id)
                    .await
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        RemoteSessionRequest::Reviewer {
            session_id,
            role,
            action,
            mut reply,
        } => {
            let result = tokio::select! {
                _ = reply.closed() => return,
                result = async {
                    manager
                        .session(session_id)
                        .await?
                        .reviewer_as(role, action)
                        .await
                } => result,
            }
            .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
    }
}

async fn serve_client(
    mut stream: TcpStream,
    metadata: DaemonMetadata,
    state: Arc<RuntimeState>,
    cancellation: CancellationToken,
) -> Result<()> {
    loop {
        let request: RequestEnvelope = match read_frame(&mut stream).await {
            Ok(request) => request,
            Err(error)
                if error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    )
                }) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let request_id = request.request_id;
        // The frozen management subset is served for every protocol version so
        // any Mjolnir build can inspect, stop, or replace this daemon; everything
        // else requires an exact protocol match.
        let is_management = matches!(
            request.action,
            DaemonAction::Ping | DaemonAction::Status | DaemonAction::Stop
        );
        let result = if request.token != metadata.token {
            Err("daemon authentication failed".to_owned())
        } else if request.protocol_version != PROTOCOL_VERSION && !is_management {
            Err(format!(
                "incompatible daemon protocol {}; expected {}",
                request.protocol_version, PROTOCOL_VERSION
            ))
        } else if cancellation.is_cancelled() && !is_management {
            // A daemon in its epilogue still holds a snapshot in memory and
            // would happily serve it, from a store it has stopped reading and
            // may no longer be able to. The retry reaches a fresh daemon,
            // which either migrates the store or reports the mismatch with the
            // numbers it read itself. Ping, Status, and Stop stay answered:
            // they touch no store, and a client asking a stopping daemon to
            // stop should not be refused.
            Err("daemon is shutting down; retry to reach a fresh daemon".to_owned())
        } else {
            let reviewer = matches!(&request.action, DaemonAction::ReviewerAction { .. });
            if reviewer {
                // A reviewer action is a long-lived sidecar operation. If its
                // client goes away, drop the future so the session actor sees
                // its reply receiver close and tears down the reviewer. A
                // one-byte peek observes EOF without consuming a pipelined
                // frame; buffered work therefore remains for the next loop.
                let mut peer_probe = [0_u8; 1];
                let mut action = Box::pin(handle_action(
                    request.action,
                    &metadata,
                    &state,
                    &cancellation,
                ));
                tokio::select! {
                    result = &mut action => result.map_err(|error| format!("{error:#}")),
                    peer = stream.peek(&mut peer_probe) => {
                        match peer {
                            Ok(0) => return Ok(()),
                            Ok(_) => action.await.map_err(|error| format!("{error:#}")),
                            Err(error) => {
                                tracing::debug!(%error, "reviewer client connection became unreadable");
                                return Ok(());
                            }
                        }
                    }
                }
            } else {
                handle_action(request.action, &metadata, &state, &cancellation)
                    .await
                    .map_err(|error| format!("{error:#}"))
            }
        };
        // Echo the caller's protocol version: replies must stay readable in the
        // client's own dialect, and the shapes it can receive here are frozen.
        write_frame(
            &mut stream,
            &ResponseEnvelope {
                protocol_version: request.protocol_version,
                request_id,
                result,
            },
        )
        .await?;
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .context("daemon background database task panicked")?
}

async fn reach_test_hook(name: &'static str) -> Result<()> {
    #[cfg(feature = "test-hooks")]
    {
        tokio::task::spawn_blocking(move || mj_core::test_hooks::reach_test_hook(name))
            .await
            .context("test hook task panicked")??;
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = name;
    Ok(())
}

async fn handle_action(
    action: DaemonAction,
    metadata: &DaemonMetadata,
    state: &Arc<RuntimeState>,
    cancellation: &CancellationToken,
) -> Result<DaemonReply> {
    match action {
        DaemonAction::Ping => Ok(DaemonReply::Pong),
        DaemonAction::Status => {
            state.prune_dead_clients();
            Ok(DaemonReply::Status(DaemonStatus {
                pid: metadata.pid,
                started_at: metadata.started_at.clone(),
                build_version: metadata.build_version.clone(),
                attached_clients: state.attachments().len(),
                phone_status: state.phone_status(),
            }))
        }
        DaemonAction::WebViewerAccess => {
            Ok(DaemonReply::WebViewerAccess(state.web_viewer.access()))
        }
        DaemonAction::RecoverWebViewer(action) => {
            state.web_viewer.recover(action)?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::InspectWebListener => {
            let address = state.web_viewer.conflict_address()?;
            let processes = blocking(move || crate::web_viewer::inspect_listener(address)).await?;
            ensure!(
                state.web_viewer.conflict_address()? == address,
                "The viewer address changed. Inspect again."
            );
            Ok(DaemonReply::WebListeners(processes))
        }
        DaemonAction::ListWorkspaces => {
            state.prune_dead_clients();
            let workspaces = blocking(crate::database::list_workspaces).await?;
            Ok(DaemonReply::Workspaces(
                workspaces
                    .into_iter()
                    .map(|workspace| WorkspaceListing { workspace })
                    .collect(),
            ))
        }
        DaemonAction::CreateWorkspace { name } => {
            // Two setup selectors can both have observed an empty workspace
            // list. The daemon operation is create-or-get so both attach to
            // the same normalized name instead of leaking a SQLite conflict.
            let workspace =
                blocking(move || crate::database::create_or_get_workspace(&name)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Workspace(workspace))
        }
        DaemonAction::RenameWorkspace { workspace_id, name } => {
            blocking(move || crate::database::rename_workspace(&workspace_id, &name)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::TouchWorkspace { workspace_id } => {
            blocking(move || crate::database::touch_workspace(&workspace_id)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DeleteWorkspace { workspace_id } => {
            ensure!(
                !state.workspace_has_active_resume(&workspace_id),
                "workspace has a session resume in progress"
            );
            blocking(move || crate::database::delete_workspace(&workspace_id)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Attach { client_id, pid } => {
            state.attachments().insert(client_id, Attachment { pid });
            state.ever_attached.store(true, Ordering::Release);
            Ok(DaemonReply::Done)
        }
        DaemonAction::Detach { client_id } => {
            state.attachments().remove(&client_id);
            Ok(DaemonReply::Done)
        }
        DaemonAction::PersistReadReceipt {
            client_id,
            workspace_id,
            session_id,
            through,
        } => {
            let frontier = blocking(move || {
                crate::database::persist_read_receipt(
                    &client_id,
                    &workspace_id,
                    &session_id,
                    through,
                )
            })
            .await?;
            Ok(DaemonReply::Ordinal(frontier))
        }
        DaemonAction::PersistDetachedSessionState {
            client_id,
            workspace_id,
            session_id,
            through,
            owner_pid,
            draft,
        } => {
            blocking(move || {
                let receipt = crate::database::persist_read_receipt(
                    &client_id,
                    &workspace_id,
                    &session_id,
                    through,
                )
                .map(|_| ());
                // Draft durability is independent of receipt validity. A
                // stale or malformed receipt must never discard typed text.
                let saved_draft = crate::database::save_detached_session_draft(
                    &workspace_id,
                    &session_id,
                    &client_id,
                    owner_pid,
                    draft,
                )
                .map(|_| ());
                receipt.and(saved_draft)
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveActiveReview { session_id, review } => {
            blocking(move || crate::database::save_active_review(&session_id, &review)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ClearActiveReview { session_id } => {
            blocking(move || crate::database::clear_active_review(&session_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RememberReviewerSelection {
            workspace_id,
            selection,
        } => {
            blocking(move || {
                crate::database::remember_reviewer_selection(&workspace_id, &selection)
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveWorkspacePaneSizes {
            workspace_id,
            sizes,
        } => {
            blocking(move || crate::database::save_workspace_pane_sizes(&workspace_id, sizes))
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::PersistImportedSession { session } => {
            blocking(move || crate::import::persist_imported_session_locally(&session)).await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetSessionTitle { session_id, title } => {
            let title =
                blocking(move || Controller::load()?.rename_session(&session_id, &title)).await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Text(title))
        }
        DaemonAction::SetSessionContainerSettings {
            session_id,
            cpus,
            memory,
            mounts,
            mount_history,
        } => {
            ensure!(
                !crate::controller::move_session::move_owns_session(&session_id),
                "session is moving; change container settings after Move finishes"
            );
            blocking(move || {
                Controller::load()?.update_session_container_settings(
                    &session_id,
                    cpus,
                    memory,
                    mounts,
                    mount_history,
                )
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetSessionAcpTitle { session_id, title } => {
            blocking(move || crate::database::set_session_acp_title(&session_id, title.as_deref()))
                .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::MarkSessionTargetMissing {
            session_id,
            detail,
            updated_at,
        } => {
            let changed = blocking(move || {
                crate::database::mark_session_target_missing(&session_id, &detail, &updated_at)
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::OptionalSessionState(changed))
        }
        DaemonAction::CheckpointSession { session_id } => Ok(DaemonReply::Checkpoint(
            state.checkpoint_session_now(&session_id).await?,
        )),
        DaemonAction::ScanRecovery => {
            let scan =
                blocking(|| Ok(Controller::load()?.scan_orphan_workers(&ProcessExecutor))).await?;
            Ok(DaemonReply::RecoveryScan(scan))
        }
        DaemonAction::AdoptRecovery {
            session_id,
            target_id,
            profile,
            bundle,
        } => {
            ensure_no_active_lifecycle(state)?;
            let mut controller = blocking(Controller::load).await?;
            controller
                .adopt_orphan_worker(
                    &session_id,
                    &target_id,
                    profile.as_deref(),
                    bundle.as_deref(),
                    &ProcessExecutor,
                )
                .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DestroyRecovery {
            session_id,
            target_id,
            confirmation,
        } => {
            ensure_no_active_lifecycle(state)?;
            blocking(move || {
                Controller::load()?.destroy_orphan_worker(
                    &session_id,
                    &target_id,
                    &confirmation,
                    &ProcessExecutor,
                )
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Snapshot { workspace_id } => {
            let snapshot = blocking(move || workspace_snapshot(&workspace_id)).await?;
            Ok(DaemonReply::Snapshot(snapshot))
        }
        DaemonAction::RuntimeSnapshot {
            workspace_id,
            after_revision,
            all_workspaces,
        } => Ok(DaemonReply::RuntimeSnapshot(Box::new(
            state
                .runtime_snapshot(&workspace_id, after_revision, all_workspaces)
                .await?,
        ))),
        DaemonAction::RenameProfile { old_id, new_id } => {
            let _config_mutation = state.config_mutation.lock().await;
            ensure_no_active_lifecycle(state)?;
            let controller = blocking(move || {
                let mut controller = Controller::load()?;
                controller.rename_profile_id(&old_id, &new_id)?;
                Ok(controller)
            })
            .await?;
            install_renamed_controller(state, controller);
            Ok(DaemonReply::Done)
        }
        DaemonAction::RenameTarget { old_id, new_id } => {
            let _config_mutation = state.config_mutation.lock().await;
            ensure_no_active_lifecycle(state)?;
            let controller = blocking(move || {
                let mut controller = Controller::load()?;
                controller.rename_target_id(&old_id, &new_id)?;
                Ok(controller)
            })
            .await?;
            install_renamed_controller(state, controller);
            Ok(DaemonReply::Done)
        }
        DaemonAction::SubmitSessionCommand {
            inherited_draft,
            session_id,
            command_id,
            command,
        } => {
            let history = if let RelayCommand::Prompt { prompt } = &command {
                let values = serde_json::to_value(prompt)?;
                let values = values
                    .as_array()
                    .context("serialized prompt content is not an array")?;
                let text = mj_core::transcript::materialized_content_text(values);
                let bundle_id = state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .state
                    .sessions
                    .get(&session_id)
                    .with_context(|| format!("unknown session {session_id}"))?
                    .bundle_id
                    .clone();
                Some((bundle_id, text))
            } else {
                None
            };
            // A completed restore is ready before the background target feed
            // has necessarily installed its new actor. Match local control
            // surfaces by awaiting that bounded handoff, not losing the first
            // command immediately after Move/Resume.
            let session = state
                .session_manager
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?;
            let session_id = session.session_id().to_owned();
            let ordinal = session.submit(command_id, command).await?;
            if let Some(expected) = inherited_draft {
                let persisted_id = session_id.clone();
                let persisted_expected = expected.clone();
                blocking(move || {
                    crate::database::clear_session_draft_input_if_matches(
                        &persisted_id,
                        &persisted_expected,
                    )
                })
                .await?;
                if let Some(record) = state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .state
                    .sessions
                    .get_mut(&session_id)
                    && record.draft_input == expected
                {
                    record.draft_input.clear();
                }
                state.publish_revision();
            }
            if let Some((bundle_id, text)) = history
                && let Err(error) = blocking(move || {
                    crate::database::record_prompt(&session_id, &bundle_id, ordinal, None, &text)
                })
                .await
            {
                tracing::warn!(%error, "prompt was accepted but its history could not be stored");
            }
            Ok(DaemonReply::Ordinal(ordinal))
        }
        DaemonAction::ReviewerAction {
            session_id,
            role,
            action,
        } => {
            let session = state.session_manager.session(session_id).await?;
            Ok(DaemonReply::Reviewer(Box::new(
                session.reviewer_as(role, action).await?,
            )))
        }
        DaemonAction::StartTurnReview { session_id } => {
            state
                .review_host()
                .start(&session_id, true)
                .await
                .map_err(|refusal| anyhow!("{refusal}"))?;
            state.publish_revision();
            Ok(DaemonReply::Done)
        }
        DaemonAction::ResolveTurnReview {
            session_id,
            resolution,
        } => {
            state
                .review_host()
                .resolve(&session_id, resolution)
                .await
                .map_err(|error| anyhow!("{error}"))?;
            state.publish_revision();
            Ok(DaemonReply::Done)
        }
        DaemonAction::SyncSession { session_id } => {
            state
                .session_manager
                .session(session_id)
                .await?
                .sync_now()
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        } => {
            state
                .session_manager
                .session(session_id)
                .await?
                .respond_elicitation(elicitation_id, response)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::StopBackgroundTask {
            session_id,
            background_task_id,
        } => {
            state
                .session_manager
                .session(session_id)
                .await?
                .stop_background_task(background_task_id)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CloseSession { session_id } => {
            state.close_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::StartCreateSession(request) => Ok(DaemonReply::RegisteredSession(Box::new(
            state.start_create_session(request).await?,
        ))),
        DaemonAction::WaitCreateSession { session_id } => {
            state.wait_create_session(&session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ResumeSession(request) => {
            state.resume_session(request).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::PrepareMoveSession(selection) => Ok(DaemonReply::MovePreparation(Box::new(
            state.prepare_move_session(selection).await?,
        ))),
        DaemonAction::MoveSession(request) => {
            Ok(DaemonReply::MoveOutcome(state.move_session(request).await?))
        }
        DaemonAction::ForceStopSession { session_id } => {
            state.force_stop_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DestroyStoppedSession { session_id } => {
            state.destroy_stopped_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ForceDestroySession { session_id } => {
            state.force_destroy_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ForceDeleteWorkspace { workspace_id } => {
            state.force_delete_workspace(workspace_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CancelLifecycle { session_id } => {
            blocking({
                let session_id = session_id.clone();
                move || crate::database::request_move_cancellation(&session_id)
            })
            .await?;
            state.cancel_lifecycle(&session_id)?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RecoverDraft { draft_id } => {
            blocking(move || crate::database::recover_detached_draft(&draft_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Stop => {
            cancellation.cancel();
            Ok(DaemonReply::Done)
        }
    }
}

/// A session has its own lifecycle operation in flight, so a fresh checkpoint
/// would fight it. Callers that only need archived state (bundle export) fall
/// back to the last durable checkpoint instead of failing (#1010).
#[derive(Debug)]
pub(crate) struct SessionLifecycleBusy {
    pub session_id: String,
}

impl std::fmt::Display for SessionLifecycleBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session {} has a lifecycle operation in flight; its checkpoint cannot run now",
            self.session_id
        )
    }
}

impl std::error::Error for SessionLifecycleBusy {}

fn ensure_no_active_lifecycle(state: &RuntimeState) -> Result<()> {
    ensure!(
        !state
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|active| active.result.borrow().is_none()),
        "cannot rename configuration while a session lifecycle operation is active"
    );
    Ok(())
}

/// The workspace's active session ids, oldest first, so force deletion
/// destroys them in a deterministic order and partial failures name what is
/// left.
fn active_sessions_for_force_destruction(
    controller: &Controller,
    workspace_id: &str,
) -> Vec<String> {
    let mut sessions: Vec<&SessionRecord> = controller
        .state
        .sessions
        .values()
        .filter(|session| session.workspace_id == workspace_id && session.state.is_active())
        .collect();
    sessions.sort_by(|a, b| a.compare_by_creation(b));
    sessions
        .into_iter()
        .map(|session| session.id.clone())
        .collect()
}

fn install_renamed_controller(state: &RuntimeState, controller: Controller) {
    *state
        .controller
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = controller;
    state.publish_revision();
}

fn workspace_snapshot(workspace_id: &str) -> Result<WorkspaceSnapshot> {
    let workspace = crate::database::list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .with_context(|| format!("unknown workspace {workspace_id:?}"))?;
    let ids = crate::database::session_ids_for_workspace(workspace_id)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let controller = Controller::load()?;
    let sessions = controller
        .state
        .sessions
        .values()
        .filter(|session| session.state.is_active() && ids.contains(&session.id))
        .map(|session| SessionPreview {
            id: session.id.clone(),
            title: session.display_title().to_owned(),
            project: session.project_name(&controller.config),
            harness: session.harness_kind.display_name().to_owned(),
            state: session.state.as_str().to_owned(),
            active: session.state.is_active(),
            updated_at: session.updated_at.clone(),
        })
        .collect();
    let drafts = crate::database::list_detached_drafts(workspace_id)?
        .into_iter()
        .map(|draft| DraftPreview {
            id: draft.id,
            session_id: draft.session_id,
            source: draft.source,
            owner_pid: draft.owner_pid,
            saved_at: draft.saved_at,
        })
        .collect();
    Ok(WorkspaceSnapshot {
        workspace,
        sessions,
        drafts,
    })
}

#[cfg(test)]
mod tests;
