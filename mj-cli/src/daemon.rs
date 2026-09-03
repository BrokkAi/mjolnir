//! Persistent per-user controller daemon and its authenticated local protocol.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use hel::hel_config::{HelConfig, data_dir};
use hel::hel_credentials::CredentialSyncSignal;
use hel::hel_database::StoreSchemaMismatch;
use hel::hel_elicitation::ElicitationResponse;
use hel::hel_review::driver::Resolution;
use hel::hel_state::{
    HostContainerSize, RecoveryObservation, RecoveryObserver, SessionRecord,
    SessionResourceAllocation, SessionState,
};
use hel::hel_targets::{
    AdditionalMount, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec,
    ProcessExecutor, ProvisionStage, ProvisionStageGuard,
};
use hel::hel_worker::{RelayCommand, RelayOperationalState};
use hel::hel_workspace::WorkspaceRecord;
use mj_controller::hel_controller::{
    Controller, ControllerStoreGuard, ResumeRepositorySourceReceipt, SessionLaunchOptions,
    SessionResumeOptions,
};
use mj_controller::hel_review_host::{RuntimeReviewView, TurnReviewHost};
use mj_controller::hel_session_manager::{
    ManagedSessionView, RemoteSessionPublisher, RemoteSessionRequest, SessionManagerChannels,
    SessionManagerControl, ViewError, spawn_remote_session_manager, spawn_session_manager,
};
use mj_controller::hel_worker_upgrade::{WorkerUpgradeObservation, WorkerUpgradeObserver};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::pollers::{
    dashboard_worker_targets, dashboard_worker_targets_excluding, interrupted_close_session_ids,
    reserve_recovery_or_cancel, spawn_image_refresher, spawn_interrupted_close_recovery,
};

pub(crate) const PROTOCOL_VERSION: u32 = 6;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a daemon is given to exit after it accepts a stop.
///
/// Stopping cancels a token and returns immediately; the daemon then unwinds
/// its session manager, its phone server and its pollers. That is normally
/// fast, but a daemon whose database has been migrated out from under it fails
/// every read while it winds down and has been observed taking over five
/// seconds — which the previous five-second bound missed by a fraction,
/// reporting a stop that had in fact worked as `did not stop` and aborting the
/// restart that depended on it.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the epilogue is given before the process leaves anyway.
///
/// Every daemon exit -- stop, SIGTERM, idle, a store that moved underneath it
/// -- unwinds through the same epilogue, and every step of it is bounded in
/// practice. This makes "the daemon did not stop" impossible rather than
/// unlikely, and it must stay well inside [`STOP_TIMEOUT`] so a client waiting
/// on a stop sees the exit rather than its own deadline.
const SHUTDOWN_FORCE_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_millis(40);

fn metadata_path() -> PathBuf {
    data_dir().join("daemon.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonMetadata {
    protocol_version: u32,
    pid: u32,
    address: SocketAddr,
    token: String,
    started_at: String,
    build_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceListing {
    pub workspace: WorkspaceRecord,
    pub attached_pids: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionPreview {
    pub id: String,
    pub title: String,
    pub project: String,
    pub harness: String,
    pub state: String,
    pub active: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceSnapshot {
    pub workspace: WorkspaceRecord,
    pub sessions: Vec<SessionPreview>,
    pub drafts: Vec<DraftPreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeSessionView {
    pub session_id: String,
    pub projection_ordinal: u64,
    pub projection_digest: String,
    pub operational: Option<RelayOperationalState>,
    pub latest_credential_sync_signal: Option<CredentialSyncSignal>,
    pub connected: bool,
    pub error: Option<ViewError>,
}

impl RuntimeSessionView {
    fn from_managed(session_id: String, view: ManagedSessionView) -> Self {
        let (projection_ordinal, projection_digest, operational, signal) =
            view.snapshot
                .map_or((0, String::new(), None, None), |snapshot| {
                    (
                        snapshot.materialized.applied_event_ordinal,
                        snapshot.materialized.applied_event_digest,
                        Some(snapshot.operational),
                        snapshot.latest_credential_sync_signal,
                    )
                });
        Self {
            session_id,
            projection_ordinal,
            projection_digest,
            operational,
            latest_credential_sync_signal: signal,
            connected: view.connected,
            error: view.error,
        }
    }
}

/// Something the daemon did on its own that a surface should report once.
///
/// Background work has no lifecycle entry to hang a message on, so notices
/// travel with the snapshot and carry an id: a surface reports the ones newer
/// than the last it saw and nothing else, however often it polls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeNotice {
    pub id: u64,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeSnapshot {
    pub revision: u64,
    pub config: HelConfig,
    pub records: Vec<SessionRecord>,
    pub sessions: Vec<RuntimeSessionView>,
    pub lifecycles: Vec<RuntimeLifecycleView>,
    /// Reviews the daemon is running, so every surface renders the same one.
    #[serde(default)]
    pub reviews: Vec<RuntimeReviewView>,
    /// Recent background events for this workspace's sessions, oldest first.
    #[serde(default)]
    pub notices: Vec<RuntimeNotice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeLifecycleKind {
    Create,
    Close,
    Resume,
    ForceStop,
    DestroyStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeLifecycleView {
    pub session_id: String,
    pub kind: RuntimeLifecycleKind,
    pub started_at_epoch_seconds: u64,
    pub active_stages: Vec<(ProvisionStage, u64)>,
    pub resume_destination: Option<(String, String)>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumeSessionRequest {
    pub session_id: String,
    pub profile_id: String,
    pub target_template_id: String,
    pub additional_mounts: Option<Vec<AdditionalMount>>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub discard_queue: bool,
    pub repository_preflight: Option<ResumeRepositorySourceReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateSessionRequest {
    pub workspace_id: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub project_directory: Option<PathBuf>,
    pub target_template_id: String,
    pub additional_mounts: Vec<AdditionalMount>,
    pub allow_dirty_local: bool,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub title: String,
    pub session_title_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisteredSession {
    pub session: SessionRecord,
    pub remembered_container_size: Option<(String, HostContainerSize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftPreview {
    pub id: String,
    pub session_id: Option<String>,
    pub source: String,
    pub owner_pid: Option<u32>,
    pub saved_at: String,
}

/// `Ping`, `Status`, and `Stop` form the frozen management subset: their wire
/// encoding — together with `RequestEnvelope`, `ResponseEnvelope`,
/// `DaemonStatus`, and `WebViewerStatus` — must never change shape, because
/// clients and daemons of *any* protocol version rely on them to identify,
/// stop, and replace each other. Every other action may change freely behind a
/// `PROTOCOL_VERSION` bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action", content = "arguments")]
enum DaemonAction {
    Ping,
    Status,
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
    },
    DeleteWorkspace {
        workspace_id: String,
    },
    Attach {
        workspace_id: String,
        client_id: String,
        pid: u32,
    },
    Detach {
        client_id: String,
    },
    PersistReadReceipt {
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
    },
    PersistDetachedSessionState {
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
        owner_pid: u32,
        draft: String,
    },
    SetSessionArchived {
        session_id: String,
        archived: bool,
    },
    SetNativeSessionHidden {
        harness: hel::hel_config::HarnessKind,
        native_session_id: String,
        hidden: bool,
    },
    SaveActiveReview {
        session_id: String,
        review: hel::hel_database::StoredReview,
    },
    ClearActiveReview {
        session_id: String,
    },
    RememberReviewerSelection {
        workspace_id: String,
        selection: hel::hel_second_opinion::ReviewerSelection,
    },
    PersistImportedSession {
        session: Box<SessionRecord>,
    },
    SetSessionTitle {
        session_id: String,
        title: String,
    },
    SetSessionContainerSettings {
        session_id: String,
        cpus: Option<String>,
        memory: Option<String>,
        mounts: Vec<AdditionalMount>,
        mount_history: Vec<PathBuf>,
    },
    SetSessionAcpTitle {
        session_id: String,
        title: Option<String>,
    },
    MarkSessionTargetMissing {
        session_id: String,
        detail: String,
        updated_at: String,
    },
    CheckpointSession {
        session_id: String,
    },
    ScanRecovery,
    AdoptRecovery {
        session_id: String,
        target_id: String,
        profile: Option<String>,
        bundle: Option<String>,
    },
    DestroyRecovery {
        session_id: String,
        target_id: String,
        confirmation: String,
    },
    Snapshot {
        workspace_id: String,
    },
    RuntimeSnapshot {
        workspace_id: String,
        after_revision: u64,
    },
    RenameProfile {
        old_id: String,
        new_id: String,
    },
    RenameTarget {
        old_id: String,
        new_id: String,
    },
    SubmitSessionCommand {
        session_id: String,
        command_id: String,
        command: RelayCommand,
    },
    SyncSession {
        session_id: String,
    },
    RespondElicitation {
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
    },
    /// Drive a session's second-opinion reviewer. The reviewer is a sidecar of
    /// the session's worker, so it travels the session's own relay rather than
    /// becoming a session of its own here.
    ReviewerAction {
        session_id: String,
        /// Which reviewing role the action drives; absent means the default
        /// one, which is what plan review uses.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        action: mj_controller::hel_session_manager::ReviewerAction,
    },
    /// Review the turn this session just finished, on a surface's request.
    StartTurnReview {
        session_id: String,
    },
    /// Forward, dismiss, or cancel the open review.
    ResolveTurnReview {
        session_id: String,
        resolution: Resolution,
    },
    CloseSession {
        session_id: String,
    },
    StartCreateSession(CreateSessionRequest),
    WaitCreateSession {
        session_id: String,
    },
    ResumeSession(ResumeSessionRequest),
    ForceStopSession {
        session_id: String,
    },
    DestroyStoppedSession {
        session_id: String,
    },
    CancelLifecycle {
        session_id: String,
    },
    RecoverDraft {
        draft_id: String,
    },
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestEnvelope {
    protocol_version: u32,
    request_id: u64,
    token: String,
    action: DaemonAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseEnvelope {
    protocol_version: u32,
    request_id: u64,
    result: std::result::Result<DaemonReply, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reply", content = "value")]
enum DaemonReply {
    Pong,
    Status(DaemonStatus),
    Workspaces(Vec<WorkspaceListing>),
    Workspace(WorkspaceRecord),
    Snapshot(WorkspaceSnapshot),
    RuntimeSnapshot(Box<RuntimeSnapshot>),
    RegisteredSession(Box<RegisteredSession>),
    Ordinal(u64),
    Text(String),
    OptionalSessionState(Option<SessionState>),
    Checkpoint(hel::hel_state::CheckpointMetadata),
    RecoveryScan(mj_controller::hel_controller::RecoveryScan),
    Reviewer(Box<mj_controller::hel_session_manager::ReviewerOutcome>),
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DaemonStatus {
    pub pid: u32,
    pub started_at: String,
    pub build_version: String,
    pub attached_clients: usize,
    pub phone_status: WebViewerStatus,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub(crate) enum WebViewerStatus {
    Disabled,
    Starting,
    Ready {
        viewer_url: String,
        viewer_code: String,
        qr_login_url: Option<String>,
        fallback_reason: Option<String>,
    },
    Stopped,
    Error {
        message: String,
    },
}

impl std::fmt::Debug for WebViewerStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready {
                viewer_url,
                viewer_code,
                fallback_reason,
                ..
            } => formatter
                .debug_struct("Ready")
                .field("viewer_url", viewer_url)
                .field("viewer_code", viewer_code)
                .field("qr_login_url", &"[redacted]")
                .field("fallback_reason", fallback_reason)
                .finish(),
            Self::Disabled => formatter.write_str("Disabled"),
            Self::Starting => formatter.write_str("Starting"),
            Self::Stopped => formatter.write_str("Stopped"),
            Self::Error { message } => formatter.debug_tuple("Error").field(message).finish(),
        }
    }
}

impl std::fmt::Display for WebViewerStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("disabled"),
            Self::Starting => formatter.write_str("starting"),
            Self::Stopped => formatter.write_str("stopped unexpectedly"),
            Self::Error { message } => write!(formatter, "error: {message}"),
            Self::Ready {
                viewer_url,
                viewer_code,
                fallback_reason,
                ..
            } => {
                write!(formatter, "{viewer_url}; viewer code {viewer_code}")?;
                if let Some(reason) = fallback_reason {
                    write!(
                        formatter,
                        "; local only because Tailscale HTTPS is unavailable: {reason}"
                    )?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Attachment {
    workspace_id: String,
    pid: u32,
}

pub(crate) struct RuntimeState {
    attachments: Mutex<BTreeMap<String, Attachment>>,
    phone_status: Mutex<WebViewerStatus>,
    ever_attached: AtomicBool,
    sessions: Mutex<BTreeMap<String, RuntimeSessionView>>,
    revisions: RuntimeRevisions,
    workspaces_tx: tokio::sync::watch::Sender<Vec<WorkspaceRecord>>,
    session_manager: SessionManagerControl,
    lifecycle: Mutex<BTreeMap<String, ActiveLifecycle>>,
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
    review_config: Arc<Mutex<hel::hel_config::ReviewConfig>>,
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
    ForceStop,
    DestroyStopped,
}

/// Whether a lifecycle has exclusive ownership of the worker target, so the
/// session manager must stop polling it. A graceful close needs the manager's
/// relay lease through checkpointing and sealing; once the durable state says
/// `Destroying`, that lease has been released and target teardown is exclusive.
fn lifecycle_owns_worker_target(kind: LifecycleKind, state: Option<SessionState>) -> bool {
    kind != LifecycleKind::Close || state == Some(SessionState::Destroying)
}

struct ActiveLifecycle {
    kind: LifecycleKind,
    cancelled: Arc<AtomicBool>,
    started_at_epoch_seconds: u64,
    active_stages: BTreeMap<ProvisionStage, (usize, u64)>,
    resume_destination: Option<(String, String)>,
    notice: Option<String>,
    result:
        tokio::sync::watch::Receiver<Option<std::result::Result<DaemonLifecycleResult, String>>>,
}

#[derive(Debug, Clone)]
enum DaemonLifecycleResult {
    Done,
}

impl From<LifecycleKind> for RuntimeLifecycleKind {
    fn from(kind: LifecycleKind) -> Self {
        match kind {
            LifecycleKind::Create => Self::Create,
            LifecycleKind::Close => Self::Close,
            LifecycleKind::Resume => Self::Resume,
            LifecycleKind::ForceStop => Self::ForceStop,
            LifecycleKind::DestroyStopped => Self::DestroyStopped,
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
            ever_attached: AtomicBool::new(false),
            sessions: Mutex::new(BTreeMap::new()),
            revisions,
            workspaces_tx,
            session_manager,
            lifecycle: Mutex::new(BTreeMap::new()),
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
    pub(crate) fn review_host(&self) -> &TurnReviewHost {
        &self.review_host
    }

    pub(crate) fn allocate_revision(&self) -> u64 {
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
                    && lifecycle_owns_worker_target(
                        active.kind,
                        controller
                            .state
                            .sessions
                            .get(*session_id)
                            .map(|session| session.state),
                    )
            })
            .map(|(session_id, _)| session_id.clone())
            .collect()
    }

    pub(crate) fn revisions(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revisions.subscribe()
    }

    /// Read the config the daemon serves right now. A task on a schedule reads
    /// it again on every tick, so a reload reaches it without a restart.
    pub(crate) fn with_config<T>(&self, read: impl FnOnce(&HelConfig) -> T) -> T {
        read(
            &self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .config,
        )
    }

    fn publish_workspaces(&self, workspaces: Vec<WorkspaceRecord>) {
        self.workspaces_tx.send_replace(workspaces);
    }

    pub(crate) async fn reload_controller(&self) -> Result<()> {
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
                let quiet = view.connected && snapshot.operational.is_quiet();
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
    ) -> Result<RuntimeSnapshot> {
        let mut revisions = self.revisions.subscribe();
        if *revisions.borrow_and_update() <= after_revision {
            let _ = tokio::time::timeout(Duration::from_secs(30), revisions.changed()).await;
        }
        let revision = self.revisions.current();
        let session_ids = blocking({
            let workspace_id = workspace_id.to_owned();
            move || hel::hel_database::session_ids_for_workspace(&workspace_id)
        })
        .await?
        .into_iter()
        .collect::<BTreeSet<_>>();
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, _)| session_ids.contains(*session_id))
            .map(|(_, view)| view.clone())
            .collect();
        let lifecycles = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, active)| {
                session_ids.contains(*session_id) && active.result.borrow().is_none()
            })
            .map(|(session_id, active)| RuntimeLifecycleView {
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
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let records = runtime_records_for_sessions(&controller, &session_ids);
        Ok(RuntimeSnapshot {
            revision,
            config: controller.config.clone(),
            records,
            sessions,
            lifecycles,
            reviews,
            notices,
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
        let mut work = Some(work);
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
                    active.kind == kind,
                    "another lifecycle operation is already running for session {session_id}"
                );
                active.result.clone()
            } else {
                let cancelled = Arc::new(AtomicBool::new(false));
                let (result_tx, result_rx) = tokio::sync::watch::channel(None);
                lifecycle.insert(
                    session_id.clone(),
                    ActiveLifecycle {
                        kind,
                        cancelled: cancelled.clone(),
                        started_at_epoch_seconds: epoch_seconds(),
                        active_stages: BTreeMap::new(),
                        resume_destination: None,
                        notice: None,
                        result: result_rx.clone(),
                    },
                );
                self.publish_revision();
                let state = Arc::clone(self);
                let operation_session_id = session_id.clone();
                let operation = work.take().expect("new lifecycle operation has work");
                tokio::spawn(async move {
                    let mut result =
                        operation(state.clone(), operation_session_id.clone(), cancelled)
                            .await
                            .map_err(|error| format!("{error:#}"));
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
                    result_tx.send_replace(Some(result));
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
            .retain(|_, active| {
                !active.result.same_channel(channel) || active.result.borrow().is_none()
            });
    }

    async fn start_create_session(
        self: &Arc<Self>,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        let registered = blocking(move || {
            let mut controller = Controller::load()?;
            let session_id = controller.register_session_with_resources(
                &request.profile_id,
                &request.bundle_id,
                &request.target_template_id,
                request.title,
                SessionLaunchOptions {
                    workspace_id: request.workspace_id,
                    additional_mounts: request.additional_mounts,
                    allow_dirty_local: request.allow_dirty_local,
                    resource_allocation: request.resource_allocation,
                    project_directory: request.project_directory,
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
                .and_then(hel::hel_config::container_size_host)
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
        self.start_or_join_lifecycle(
            session_id,
            LifecycleKind::Create,
            |state, session_id, cancelled| async move {
                let mut controller = tokio::task::spawn_blocking(Controller::load)
                    .await
                    .context("load controller for daemon create task")??;
                let executor = DaemonStageReportingExecutor::new(
                    CancellableProcessExecutor::new(cancelled),
                    state,
                    session_id.clone(),
                );
                controller
                    .provision_session_controlled(&session_id, &executor)
                    .await?;
                Ok(DaemonLifecycleResult::Done)
            },
        )?;
        Ok(registered)
    }

    async fn wait_create_session(&self, session_id: &str) -> Result<()> {
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
        }
    }

    pub(crate) async fn close_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        let already_stopped = blocking({
            let session_id = session_id.clone();
            move || {
                let controller = Controller::load()?;
                Ok(controller
                    .state
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.state == SessionState::Stopped))
            }
        })
        .await?;
        if already_stopped {
            return Ok(());
        }
        self.run_lifecycle(
            session_id,
            LifecycleKind::Close,
            |state, session_id, cancelled| async move {
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
                controller
                    .close_session_managed_controlled(
                        &session_id,
                        &executor,
                        &state.session_manager,
                    )
                    .await?;
                Ok(DaemonLifecycleResult::Done)
            },
        )
        .await?;
        Ok(())
    }

    /// Resume a session, and return nothing.
    ///
    /// This used to answer with the whole `MaterializedSession`. That reply
    /// travels as one JSON frame against `MAX_FRAME_BYTES`, so a session whose
    /// projection outgrew 8 MiB could not be resumed at all — it built a
    /// several-hundred-megabyte buffer and then refused to send it. The
    /// projection is already durable; a viewer reads it from the store.
    pub(crate) async fn resume_session(
        self: &Arc<Self>,
        request: ResumeSessionRequest,
    ) -> Result<()> {
        let session_id = request.session_id.clone();
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
        let operation_session_id = session_id.clone();
        let result = self.start_or_join_lifecycle(
            session_id,
            LifecycleKind::Resume,
            move |state, session_id, cancelled| async move {
                let _recovery_reservation = tokio::task::spawn_blocking({
                    let observer = state.recovery_observer.clone();
                    let session_id = session_id.clone();
                    let cancelled = cancelled.clone();
                    move || reserve_recovery_or_cancel(&observer, &session_id, &cancelled)
                })
                .await
                .context("reserve recovery for daemon resume task")??;
                let mut controller = tokio::task::spawn_blocking(Controller::load)
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
        let DaemonLifecycleResult::Done = result?;
        Ok(())
    }

    async fn force_stop_session(self: &Arc<Self>, session_id: String) -> Result<()> {
        self.run_lifecycle(
            session_id,
            LifecycleKind::ForceStop,
            |state, session_id, cancelled| async move {
                blocking(move || {
                    let mut controller = Controller::load()?;
                    let executor = DaemonStageReportingExecutor::new(
                        CancellableProcessExecutor::new(cancelled),
                        state,
                        session_id.clone(),
                    );
                    controller.force_stop(&session_id, &executor)?;
                    Ok(DaemonLifecycleResult::Done)
                })
                .await
            },
        )
        .await?;
        Ok(())
    }

    async fn destroy_stopped_session(self: &Arc<Self>, session_id: String) -> Result<()> {
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

    fn cancel_lifecycle(&self, session_id: &str) -> Result<()> {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let active = lifecycle.get(session_id).with_context(|| {
            format!("no lifecycle operation is running for session {session_id}")
        })?;
        active.cancelled.store(true, Ordering::Release);
        Ok(())
    }

    /// Cancel and join every lifecycle owner before the daemon closes its
    /// session manager and database writer. The operations already run
    /// concurrently; awaiting their result feeds sequentially only observes
    /// completion and never serializes the work itself.
    async fn cancel_and_wait_lifecycles(&self) -> Result<()> {
        let pending = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            lifecycle
                .iter()
                .filter(|(_, active)| active.result.borrow().is_none())
                .map(|(session_id, active)| {
                    active.cancelled.store(true, Ordering::Release);
                    (session_id.clone(), active.result.clone())
                })
                .collect::<Vec<_>>()
        };
        for (session_id, mut result) in pending {
            while result.borrow_and_update().is_none() {
                result.changed().await.with_context(|| {
                    format!("lifecycle owner stopped without a result for session {session_id}")
                })?;
            }
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
    pub(crate) fn active_lifecycles(&self) -> Vec<RuntimeLifecycleView> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, active)| active.result.borrow().is_none())
            .map(|(session_id, active)| RuntimeLifecycleView {
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

    pub(crate) fn cancel_lifecycle_if_active(&self, session_id: &str) {
        if let Some(active) = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
        {
            active.cancelled.store(true, Ordering::Release);
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
            active.notice = Some(notice.to_owned());
            self.publish_revision();
        }
    }
}

/// Log one finished worker upgrade, and tell the surfaces about the one that
/// changed something.
fn report_worker_upgrade(
    state: &RuntimeState,
    result: &mj_controller::hel_worker_upgrade::WorkerUpgradeResult,
) {
    use mj_controller::hel_controller::WorkerUpgradeOutcome;

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

fn runtime_records_for_sessions(
    controller: &Controller,
    session_ids: &BTreeSet<String>,
) -> Vec<SessionRecord> {
    controller
        .state
        .sessions
        .iter()
        .filter(|(session_id, _)| session_ids.contains(*session_id))
        .map(|(_, session)| session.clone())
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
        self.inner.execute(command)
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        let _stage = command
            .stage
            .map(|stage| ProvisionStageGuard::new(self, stage));
        self.inner.execute_with_stdin(command, input)
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

/// Whether a non-child process has exited but not yet been reaped.
///
/// A zombie still answers `kill(pid, 0)`, because its process-table entry
/// survives until its parent waits for it — so an existence probe alone calls
/// it alive forever and anything waiting for it to leave waits forever. That is
/// exactly the shape of `mj daemon restart` refusing to restart a daemon that
/// had already stopped: `spawn_detached` used to leave the daemon a child of a
/// long-lived Mjolnir process that never reaped it. It now double-forks, so the
/// daemon is init's to reap, but any other unreaped child of this process would
/// look the same, and the check stays cheap.
///
/// Treating a zombie as gone is also safe in the direction that matters: a
/// zombie's PID cannot be reused until it is reaped, so nothing else can be
/// occupying that number while this returns true.
#[cfg(unix)]
fn process_is_zombie(pid: u32) -> bool {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .is_some_and(|process| process.status() == sysinfo::ProcessStatus::Zombie)
}

/// Wait for a process to leave, within [`STOP_TIMEOUT`].
///
/// The error says the process was still running rather than that it "did not
/// stop": a daemon that is still winding down has not refused, and the two
/// read very differently to somebody deciding whether to reach for a kill.
async fn wait_for_exit(pid: u32) -> Result<()> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while daemon_process_is_alive(pid) {
        ensure!(Instant::now() < deadline, "process {pid} is still running");
        tokio::time::sleep(RETRY_DELAY).await;
    }
    Ok(())
}

/// Whether a daemon that Mjolnir launched in its own process group is alive.
///
/// Reaping is deliberately confined to this daemon-specific path. Attachment
/// PIDs are merely observations and may alias unrelated children owned by this
/// process, so their liveness probe below must never call `waitpid`.
fn daemon_process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        let mut status = 0;
        // SAFETY: `status` is writable for the call and WNOHANG never blocks.
        // A non-child fails with ECHILD without changing any process state.
        let waited = unsafe { libc::waitpid(raw_pid, &mut status, libc::WNOHANG) };
        if waited == raw_pid {
            return false;
        }
        if waited == 0 {
            return true;
        }
        let wait_error = std::io::Error::last_os_error();
        if wait_error.raw_os_error() != Some(libc::ECHILD) {
            return true;
        }

        #[cfg(target_os = "macos")]
        return owned_daemon_group_is_alive(raw_pid);

        #[cfg(not(target_os = "macos"))]
        process_is_alive(pid)
    }
    #[cfg(not(unix))]
    process_is_alive(pid)
}

#[cfg(target_os = "macos")]
fn owned_daemon_group_is_alive(pid: libc::pid_t) -> bool {
    // `spawn_detached` makes the daemon a process-group leader. Darwin
    // excludes zombies from group signal probes: ESRCH means the group is gone
    // and EPERM means only exiting members remain. The latter is safe here
    // because this is a group we created for our own same-user child, not an
    // arbitrary process group.
    // SAFETY: signal 0 is only an existence probe, and the negative PID targets
    // the daemon-owned group rather than another process.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    !matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::EPERM))
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: kill(pid, 0) sends no signal and is the standard existence
        // probe. EPERM still means the process exists.
        let result = unsafe { libc::kill(raw_pid, 0) };
        let exists =
            result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        exists && !process_is_zombie(pid)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
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

fn read_metadata() -> Result<DaemonMetadata> {
    let metadata = read_metadata_any()?;
    ensure!(
        metadata.protocol_version == PROTOCOL_VERSION,
        "daemon protocol {} is incompatible with client protocol {}",
        metadata.protocol_version,
        PROTOCOL_VERSION
    );
    Ok(metadata)
}

fn read_metadata_any() -> Result<DaemonMetadata> {
    let path = metadata_path();
    let body = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let metadata: DaemonMetadata =
        serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))?;
    Ok(metadata)
}

async fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    ensure!(body.len() <= MAX_FRAME_BYTES, "daemon frame is too large");
    stream.write_u32(body.len() as u32).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    ensure!(
        length <= MAX_FRAME_BYTES,
        "daemon frame exceeds {MAX_FRAME_BYTES} bytes"
    );
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("decode daemon frame")
}

pub(crate) struct DaemonClient {
    metadata: DaemonMetadata,
    stream: TcpStream,
    next_request_id: u64,
}

impl DaemonClient {
    async fn connect(metadata: DaemonMetadata) -> Result<Self> {
        let stream =
            tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(metadata.address))
                .await
                .context("time out connecting to Mjolnir daemon")??;
        Ok(Self {
            metadata,
            stream,
            next_request_id: 1,
        })
    }

    /// Speak the daemon's advertised dialect, not this build's: management
    /// requests must reach daemons of any protocol version, and the frozen
    /// subset encodes identically across all of them.
    async fn request(&mut self, action: DaemonAction) -> Result<DaemonReply> {
        let protocol_version = self.metadata.protocol_version;
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(
            &mut self.stream,
            &RequestEnvelope {
                protocol_version,
                request_id,
                token: self.metadata.token.clone(),
                action,
            },
        )
        .await?;
        let response: ResponseEnvelope = read_frame(&mut self.stream).await?;
        ensure!(
            response.protocol_version == protocol_version,
            "daemon changed protocol"
        );
        ensure!(
            response.request_id == request_id,
            "daemon crossed request IDs"
        );
        response.result.map_err(anyhow::Error::msg)
    }

    pub(crate) async fn status(&mut self) -> Result<DaemonStatus> {
        match self.request(DaemonAction::Status).await? {
            DaemonReply::Status(status) => Ok(status),
            reply => bail!("unexpected daemon status reply {reply:?}"),
        }
    }

    pub(crate) async fn list_workspaces(&mut self) -> Result<Vec<WorkspaceListing>> {
        match self.request(DaemonAction::ListWorkspaces).await? {
            DaemonReply::Workspaces(workspaces) => Ok(workspaces),
            reply => bail!("unexpected daemon workspace reply {reply:?}"),
        }
    }

    pub(crate) async fn rename_profile(&mut self, old_id: String, new_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameProfile { old_id, new_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-profile reply {reply:?}"),
        }
    }

    pub(crate) async fn rename_target(&mut self, old_id: String, new_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameTarget { old_id, new_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-target reply {reply:?}"),
        }
    }

    pub(crate) async fn create_workspace(&mut self, name: String) -> Result<WorkspaceRecord> {
        match self.request(DaemonAction::CreateWorkspace { name }).await? {
            DaemonReply::Workspace(workspace) => Ok(workspace),
            reply => bail!("unexpected create-workspace reply {reply:?}"),
        }
    }

    pub(crate) async fn rename_workspace(
        &mut self,
        workspace_id: String,
        name: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::RenameWorkspace { workspace_id, name })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-workspace reply {reply:?}"),
        }
    }

    pub(crate) async fn delete_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::DeleteWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected delete-workspace reply {reply:?}"),
        }
    }

    pub(crate) async fn attach(
        &mut self,
        workspace_id: String,
        client_id: String,
        pid: u32,
    ) -> Result<()> {
        match self
            .request(DaemonAction::Attach {
                workspace_id,
                client_id,
                pid,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected attach reply {reply:?}"),
        }
    }

    pub(crate) async fn detach(&mut self, client_id: String) -> Result<()> {
        match self.request(DaemonAction::Detach { client_id }).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected detach reply {reply:?}"),
        }
    }

    pub(crate) async fn persist_read_receipt(
        &mut self,
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
    ) -> Result<u64> {
        match self
            .request(DaemonAction::PersistReadReceipt {
                client_id,
                workspace_id,
                session_id,
                through,
            })
            .await?
        {
            DaemonReply::Ordinal(ordinal) => Ok(ordinal),
            reply => bail!("unexpected read-receipt reply {reply:?}"),
        }
    }

    pub(crate) async fn persist_detached_session_state(
        &mut self,
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
        owner_pid: u32,
        draft: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::PersistDetachedSessionState {
                client_id,
                workspace_id,
                session_id,
                through,
                owner_pid,
                draft,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected detached-session-state reply {reply:?}"),
        }
    }

    pub(crate) async fn set_session_archived(
        &mut self,
        session_id: String,
        archived: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SetSessionArchived {
                session_id,
                archived,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected session-archive reply {reply:?}"),
        }
    }

    pub(crate) async fn set_native_session_hidden(
        &mut self,
        harness: hel::hel_config::HarnessKind,
        native_session_id: String,
        hidden: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SetNativeSessionHidden {
                harness,
                native_session_id,
                hidden,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected native-session visibility reply {reply:?}"),
        }
    }

    pub(crate) async fn save_active_review(
        &mut self,
        session_id: String,
        review: hel::hel_database::StoredReview,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SaveActiveReview { session_id, review })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected save-review reply {reply:?}"),
        }
    }

    pub(crate) async fn clear_active_review(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ClearActiveReview { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected clear-review reply {reply:?}"),
        }
    }

    pub(crate) async fn remember_reviewer_selection(
        &mut self,
        workspace_id: String,
        selection: hel::hel_second_opinion::ReviewerSelection,
    ) -> Result<()> {
        match self
            .request(DaemonAction::RememberReviewerSelection {
                workspace_id,
                selection,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected reviewer-selection reply {reply:?}"),
        }
    }

    pub(crate) async fn persist_imported_session(&mut self, session: SessionRecord) -> Result<()> {
        match self
            .request(DaemonAction::PersistImportedSession {
                session: Box::new(session),
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected imported-session reply {reply:?}"),
        }
    }

    pub(crate) async fn set_session_title(
        &mut self,
        session_id: String,
        title: String,
    ) -> Result<String> {
        match self
            .request(DaemonAction::SetSessionTitle { session_id, title })
            .await?
        {
            DaemonReply::Text(title) => Ok(title),
            reply => bail!("unexpected session-title reply {reply:?}"),
        }
    }

    pub(crate) async fn set_session_container_settings(
        &mut self,
        session_id: String,
        cpus: Option<String>,
        memory: Option<String>,
        mounts: Vec<AdditionalMount>,
        mount_history: Vec<PathBuf>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SetSessionContainerSettings {
                session_id,
                cpus,
                memory,
                mounts,
                mount_history,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected container-settings reply {reply:?}"),
        }
    }

    pub(crate) async fn set_session_acp_title(
        &mut self,
        session_id: String,
        title: Option<String>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SetSessionAcpTitle { session_id, title })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected ACP-title reply {reply:?}"),
        }
    }

    pub(crate) async fn mark_session_target_missing(
        &mut self,
        session_id: String,
        detail: String,
        updated_at: String,
    ) -> Result<Option<SessionState>> {
        match self
            .request(DaemonAction::MarkSessionTargetMissing {
                session_id,
                detail,
                updated_at,
            })
            .await?
        {
            DaemonReply::OptionalSessionState(state) => Ok(state),
            reply => bail!("unexpected target-missing reply {reply:?}"),
        }
    }

    pub(crate) async fn checkpoint_session(
        &mut self,
        session_id: String,
    ) -> Result<hel::hel_state::CheckpointMetadata> {
        match self
            .request(DaemonAction::CheckpointSession { session_id })
            .await?
        {
            DaemonReply::Checkpoint(checkpoint) => Ok(checkpoint),
            reply => bail!("unexpected checkpoint reply {reply:?}"),
        }
    }

    pub(crate) async fn scan_recovery(
        &mut self,
    ) -> Result<mj_controller::hel_controller::RecoveryScan> {
        match self.request(DaemonAction::ScanRecovery).await? {
            DaemonReply::RecoveryScan(scan) => Ok(scan),
            reply => bail!("unexpected recovery-scan reply {reply:?}"),
        }
    }

    pub(crate) async fn adopt_recovery(
        &mut self,
        session_id: String,
        target_id: String,
        profile: Option<String>,
        bundle: Option<String>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::AdoptRecovery {
                session_id,
                target_id,
                profile,
                bundle,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected recovery-adopt reply {reply:?}"),
        }
    }

    pub(crate) async fn destroy_recovery(
        &mut self,
        session_id: String,
        target_id: String,
        confirmation: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::DestroyRecovery {
                session_id,
                target_id,
                confirmation,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected recovery-destroy reply {reply:?}"),
        }
    }

    pub(crate) async fn snapshot(&mut self, workspace_id: String) -> Result<WorkspaceSnapshot> {
        match self
            .request(DaemonAction::Snapshot { workspace_id })
            .await?
        {
            DaemonReply::Snapshot(snapshot) => Ok(snapshot),
            reply => bail!("unexpected snapshot reply {reply:?}"),
        }
    }

    pub(crate) async fn runtime_snapshot(
        &mut self,
        workspace_id: String,
        after_revision: u64,
    ) -> Result<RuntimeSnapshot> {
        match self
            .request(DaemonAction::RuntimeSnapshot {
                workspace_id,
                after_revision,
            })
            .await?
        {
            DaemonReply::RuntimeSnapshot(snapshot) => Ok(*snapshot),
            reply => bail!("unexpected runtime snapshot reply {reply:?}"),
        }
    }

    pub(crate) async fn submit_session_command(
        &mut self,
        session_id: String,
        command_id: String,
        command: RelayCommand,
    ) -> Result<u64> {
        match self
            .request(DaemonAction::SubmitSessionCommand {
                session_id,
                command_id,
                command,
            })
            .await?
        {
            DaemonReply::Ordinal(ordinal) => Ok(ordinal),
            reply => bail!("unexpected session command reply {reply:?}"),
        }
    }

    /// Ask the daemon to review the turn this session just finished.
    ///
    /// The refusal is a sentence for a person -- "prompts are queued", "set
    /// [review] profile in config.toml" -- so it travels as text rather than
    /// as a code every surface would have to translate.
    pub(crate) async fn start_turn_review(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::StartTurnReview { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected turn-review reply {reply:?}"),
        }
    }

    pub(crate) async fn resolve_turn_review(
        &mut self,
        session_id: String,
        resolution: Resolution,
    ) -> Result<()> {
        match self
            .request(DaemonAction::ResolveTurnReview {
                session_id,
                resolution,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected turn-review resolution reply {reply:?}"),
        }
    }

    pub(crate) async fn reviewer_action(
        &mut self,
        session_id: String,
        role: Option<String>,
        action: mj_controller::hel_session_manager::ReviewerAction,
    ) -> Result<mj_controller::hel_session_manager::ReviewerOutcome> {
        match self
            .request(DaemonAction::ReviewerAction {
                session_id,
                role,
                action,
            })
            .await?
        {
            DaemonReply::Reviewer(outcome) => Ok(*outcome),
            reply => bail!("unexpected reviewer reply {reply:?}"),
        }
    }

    pub(crate) async fn sync_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::SyncSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected session sync reply {reply:?}"),
        }
    }

    pub(crate) async fn respond_elicitation(
        &mut self,
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        match self
            .request(DaemonAction::RespondElicitation {
                session_id,
                elicitation_id,
                response,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected elicitation reply {reply:?}"),
        }
    }

    pub(crate) async fn close_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CloseSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected close-session reply {reply:?}"),
        }
    }

    pub(crate) async fn start_create_session(
        &mut self,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        match self
            .request(DaemonAction::StartCreateSession(request))
            .await?
        {
            DaemonReply::RegisteredSession(registered) => Ok(*registered),
            reply => bail!("unexpected start-create reply {reply:?}"),
        }
    }

    pub(crate) async fn wait_create_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::WaitCreateSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected wait-create reply {reply:?}"),
        }
    }

    pub(crate) async fn resume_session(&mut self, request: ResumeSessionRequest) -> Result<()> {
        match self.request(DaemonAction::ResumeSession(request)).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected resume-session reply {reply:?}"),
        }
    }

    pub(crate) async fn force_stop_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ForceStopSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected force-stop reply {reply:?}"),
        }
    }

    pub(crate) async fn destroy_stopped_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::DestroyStoppedSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected destroy-stopped reply {reply:?}"),
        }
    }

    pub(crate) async fn cancel_lifecycle(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CancelLifecycle { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected cancel-lifecycle reply {reply:?}"),
        }
    }

    pub(crate) async fn recover_draft(&mut self, draft_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RecoverDraft { draft_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected recover-draft reply {reply:?}"),
        }
    }

    pub(crate) async fn stop(&mut self) -> Result<()> {
        match self.request(DaemonAction::Stop).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected stop reply {reply:?}"),
        }
    }
}

pub(crate) async fn connect_existing() -> Result<DaemonClient> {
    DaemonClient::connect(read_metadata()?).await
}

/// A handle to whatever daemon the metadata file advertises, regardless of its
/// protocol version. It only exposes the frozen management subset (`Ping`,
/// `Status`, `Stop`), which encodes identically in every protocol version.
pub(crate) struct ManagementClient {
    inner: DaemonClient,
}

impl ManagementClient {
    pub(crate) fn protocol_version(&self) -> u32 {
        self.inner.metadata.protocol_version
    }

    pub(crate) async fn status(&mut self) -> Result<DaemonStatus> {
        self.inner.status().await
    }

    pub(crate) async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }

    /// Ask the daemon to stop and wait for its process to actually exit.
    pub(crate) async fn stop_and_wait(mut self) -> Result<()> {
        let pid = self.inner.metadata.pid;
        self.inner.stop().await?;
        wait_for_exit(pid).await.with_context(|| {
            format!(
                "Mjolnir daemon {pid} accepted the stop but was still running after {}s",
                STOP_TIMEOUT.as_secs()
            )
        })
    }
}

pub(crate) async fn connect_management() -> Result<ManagementClient> {
    Ok(ManagementClient {
        inner: DaemonClient::connect(read_metadata_any()?).await?,
    })
}

pub(crate) async fn connect_or_start() -> Result<DaemonClient> {
    if let Ok(metadata) = read_metadata_any()
        && metadata.protocol_version != PROTOCOL_VERSION
    {
        replace_incompatible_daemon(&metadata).await?;
    }
    if let Ok(mut client) = connect_existing().await
        && matches!(
            client.request(DaemonAction::Ping).await,
            Ok(DaemonReply::Pong)
        )
    {
        return Ok(client);
    }

    let executable = std::env::current_exe().context("find current mj executable")?;
    let mut command = std::process::Command::new(executable);
    command.arg("daemon-run");
    let _pid = hel::hel_subprocess::spawn_detached(&mut command, &data_dir().join("daemon.log"))?;

    let deadline = Instant::now() + START_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match connect_existing().await {
            Ok(mut client) => match client.request(DaemonAction::Ping).await {
                Ok(DaemonReply::Pong) => return Ok(client),
                Ok(reply) => last_error = Some(anyhow!("unexpected startup reply {reply:?}")),
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow!("Mjolnir daemon did not become ready"))).with_context(
        || {
            format!(
                "start Mjolnir daemon; details are in {}",
                data_dir().join("daemon.log").display()
            )
        },
    )
}

/// Clear the way for a daemon speaking this build's protocol. Ask the running
/// daemon to stop over the frozen management subset first — graceful for every
/// protocol version — and only signal it when the wire is unreachable.
async fn replace_incompatible_daemon(metadata: &DaemonMetadata) -> Result<()> {
    if let Ok(inner) = DaemonClient::connect(metadata.clone()).await
        && (ManagementClient { inner }).stop_and_wait().await.is_ok()
    {
        return Ok(());
    }
    signal_incompatible_daemon(metadata).await
}

async fn signal_incompatible_daemon(metadata: &DaemonMetadata) -> Result<()> {
    #[cfg(unix)]
    {
        let mut system = sysinfo::System::new();
        system.refresh_processes(
            sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(metadata.pid)]),
            true,
        );
        // The PID comes from the owner-only metadata file; the argv check
        // guards against PID recycling, not against other Mjolnir builds — old
        // daemons are exactly what this function exists to retire.
        let is_hel_daemon = system
            .process(sysinfo::Pid::from_u32(metadata.pid))
            .is_some_and(|process| {
                process
                    .cmd()
                    .get(1)
                    .is_some_and(|argument| argument.to_str() == Some("daemon-run"))
            });
        ensure!(
            is_hel_daemon,
            "refusing to signal PID {} because it does not look like a Mjolnir daemon (`mj daemon-run`)",
            metadata.pid
        );
        // SAFETY: the PID comes from owner-only daemon metadata and SIGTERM is
        // handled as graceful cancellation by every supported daemon.
        let result = unsafe { libc::kill(metadata.pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error).context("stop incompatible Mjolnir daemon");
            }
        }
        wait_for_exit(metadata.pid).await.with_context(|| {
            format!(
                "incompatible Mjolnir daemon {} was signalled but was still running after {}s",
                metadata.pid,
                STOP_TIMEOUT.as_secs()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        bail!("stop the incompatible Mjolnir daemon, then retry")
    }
}

pub(crate) fn maintain_attachment(
    workspace_id: String,
    client_id: String,
    pid: u32,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    match connect_or_start().await {
                        Ok(mut daemon) => {
                            if let Err(error) = daemon
                                .attach(workspace_id.clone(), client_id.clone(), pid)
                                .await
                            {
                                tracing::warn!(%error, "could not refresh daemon workspace attachment");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "could not reconnect dashboard to Mjolnir daemon");
                        }
                    }
                }
            }
        }
    })
}

pub(crate) async fn run_daemon_process() -> Result<()> {
    let guard = ControllerStoreGuard::acquire()?;
    let database_writer = guard.start_database_writer()?;
    let epilogue_started = AtomicBool::new(false);
    let mut outcome = run_daemon_runtime(&epilogue_started).await;
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

async fn run_daemon_runtime(epilogue_started: &AtomicBool) -> Result<()> {
    Controller::recover_config_id_rename()?;
    HelConfig::migrate_legacy_localhost_target()?;
    let config = HelConfig::load()?;
    hel::hel_database::recover_interrupted_checkpointing_sessions(
        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )?;
    mj_controller::hel_controller::reconcile_managed_checkpoint_archives()?;

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
    let workspaces = tokio::task::spawn_blocking(hel::hel_database::list_workspaces)
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
    let mut recovery =
        mj_controller::hel_recovery::RecoveryCoordinator::spawn(manager_control.clone());
    let recovery_observer = recovery.observer();
    // Shares the recovery gate, so a recovery copy and a worker upgrade never
    // act on one session at the same time.
    let mut worker_upgrades = mj_controller::hel_worker_upgrade::WorkerUpgradeCoordinator::spawn(
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
    let cancellation = hel::termination::Coordinator::install().token();
    let target_refresh = spawn_manager_target_refresher(
        manager_targets.clone(),
        cancellation.clone(),
        state.clone(),
    );
    let image_refresh = spawn_image_refresher(
        {
            let state = state.clone();
            move || state.with_config(mj_controller::hel_controller::image_refresh_plan)
        },
        cancellation.clone(),
    );
    let exit_when_idle = hel::hel_config::env_override_os("DAEMON_EXIT_WHEN_IDLE").is_some();
    let mut idle_tick = tokio::time::interval(Duration::from_millis(100));
    idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut recovery_tick = tokio::time::interval(Duration::from_millis(250));
    recovery_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (interrupted_close_tx, mut interrupted_close_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut interrupted_close_cancellations = Vec::new();
    let mut interrupted_close_tasks = Vec::new();
    for session_id in interrupted_close_session_ids(&controller) {
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
                        if let Err(error) = completed.result {
                            tracing::warn!(session_id = %completed.session_id, %error, "daemon could not resume interrupted close");
                        }
                        refresh_runtime_controller(&state).await;
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
    targets: tokio::sync::watch::Sender<
        Vec<mj_controller::hel_session_manager::RelaySessionTarget>,
    >,
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
                            targets.send_replace(refreshed);
                            if changed {
                                state.publish_revision();
                            }
                        }
                        Ok(Err(error)) => {
                            // The one place divergence is classified. Every
                            // read this daemon makes re-checks the recorded
                            // schema, so a store migrated by another process
                            // arrives here within one tick. A daemon that
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
    let workspaces = tokio::task::spawn_blocking(hel::hel_database::list_workspaces)
        .await
        .context("daemon workspace refresh task panicked")??;
    state.publish_workspaces(workspaces);
    Ok(())
}

fn spawn_phone_server(
    config: hel::hel_config::PhoneConfig,
    cancellation: CancellationToken,
    state: Arc<RuntimeState>,
    worker: SessionManagerChannels,
) -> tokio::task::JoinHandle<()> {
    state.set_phone_status(WebViewerStatus::Starting);
    let workspaces = state.workspaces();
    tokio::spawn(async move {
        let reporter = {
            let state = state.clone();
            move |status| state.set_phone_status(status)
        };
        match crate::server::run_server(
            (&config).into(),
            cancellation.clone(),
            reporter,
            worker,
            state.clone(),
            workspaces,
        )
        .await
        {
            Ok(()) if cancellation.is_cancelled() => {}
            Ok(()) => state.set_phone_status(WebViewerStatus::Stopped),
            Err(error) => {
                tracing::warn!(error = format!("{error:#}"), "phone server stopped");
                state.set_phone_status(WebViewerStatus::Error {
                    message: format!("{error:#}"),
                });
            }
        }
    })
}

fn spawn_remote_request_bridge(
    mut requests: mj_controller::hel_session_manager::RemoteSessionRequests,
    manager: SessionManagerControl,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // One session's requests reach its relay actor in the order they were
        // made; different sessions still overlap.
        let mut request_order = mj_controller::hel_session_manager::SessionRequestOrder::new();
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
            reply,
        } => {
            let result = async {
                manager
                    .session(session_id)
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
        RemoteSessionRequest::Reviewer {
            session_id,
            role,
            action,
            reply,
        } => {
            let result = async {
                manager
                    .session(session_id)
                    .await?
                    .reviewer_as(role, action)
                    .await
            }
            .await
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
            handle_action(request.action, &metadata, &state, &cancellation)
                .await
                .map_err(|error| format!("{error:#}"))
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
        tokio::task::spawn_blocking(move || hel::hel_test_hooks::reach_test_hook(name))
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
        DaemonAction::ListWorkspaces => {
            state.prune_dead_clients();
            let workspaces = blocking(hel::hel_database::list_workspaces).await?;
            let attachments = state.attachments();
            Ok(DaemonReply::Workspaces(
                workspaces
                    .into_iter()
                    .map(|workspace| {
                        let attached_pids = attachments
                            .values()
                            .filter(|attachment| attachment.workspace_id == workspace.id)
                            .map(|attachment| attachment.pid)
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        WorkspaceListing {
                            workspace,
                            attached_pids,
                        }
                    })
                    .collect(),
            ))
        }
        DaemonAction::CreateWorkspace { name } => {
            // Two setup selectors can both have observed an empty workspace
            // list. The daemon operation is create-or-get so both attach to
            // the same normalized name instead of leaking a SQLite conflict.
            let workspace =
                blocking(move || hel::hel_database::create_or_get_workspace(&name)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Workspace(workspace))
        }
        DaemonAction::RenameWorkspace { workspace_id, name } => {
            blocking(move || hel::hel_database::rename_workspace(&workspace_id, &name)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DeleteWorkspace { workspace_id } => {
            ensure!(
                !state
                    .attachments()
                    .values()
                    .any(|attachment| attachment.workspace_id == workspace_id),
                "workspace still has attached clients"
            );
            blocking(move || hel::hel_database::delete_empty_workspace(&workspace_id)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Attach {
            workspace_id,
            client_id,
            pid,
        } => {
            let exists = blocking({
                let workspace_id = workspace_id.clone();
                move || {
                    Ok(hel::hel_database::list_workspaces()?
                        .iter()
                        .any(|workspace| workspace.id == workspace_id))
                }
            })
            .await?;
            ensure!(exists, "unknown workspace {workspace_id:?}");
            let changed = state
                .attachments()
                .insert(
                    client_id,
                    Attachment {
                        workspace_id: workspace_id.clone(),
                        pid,
                    },
                )
                .is_none_or(|previous| {
                    previous.workspace_id != workspace_id || previous.pid != pid
                });
            state.ever_attached.store(true, Ordering::Release);
            if changed {
                blocking(move || hel::hel_database::touch_workspace(&workspace_id)).await?;
            }
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
                hel::hel_database::persist_read_receipt(
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
                let receipt = hel::hel_database::persist_read_receipt(
                    &client_id,
                    &workspace_id,
                    &session_id,
                    through,
                )
                .map(|_| ());
                // Draft durability is independent of receipt validity. A
                // stale or malformed receipt must never discard typed text.
                let saved_draft = hel::hel_database::save_detached_draft(
                    &workspace_id,
                    Some(&session_id),
                    &client_id,
                    Some(owner_pid),
                    &draft,
                )
                .map(|_| ());
                receipt.and(saved_draft)
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetSessionArchived {
            session_id,
            archived,
        } => {
            blocking(move || hel::hel_database::set_session_archived(&session_id, archived))
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetNativeSessionHidden {
            harness,
            native_session_id,
            hidden,
        } => {
            blocking(move || {
                hel::hel_database::set_native_session_hidden(harness, &native_session_id, hidden)
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveActiveReview { session_id, review } => {
            blocking(move || hel::hel_database::save_active_review(&session_id, &review)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ClearActiveReview { session_id } => {
            blocking(move || hel::hel_database::clear_active_review(&session_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RememberReviewerSelection {
            workspace_id,
            selection,
        } => {
            blocking(move || {
                hel::hel_database::remember_reviewer_selection(&workspace_id, &selection)
            })
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
            blocking(move || {
                hel::hel_database::set_session_acp_title(&session_id, title.as_deref())
            })
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
                hel::hel_database::mark_session_target_missing(&session_id, &detail, &updated_at)
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::OptionalSessionState(changed))
        }
        DaemonAction::CheckpointSession { session_id } => {
            ensure_no_active_lifecycle(state)?;
            let mut controller = blocking(Controller::load).await?;
            let checkpoint = controller.checkpoint_session(&session_id).await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Checkpoint(checkpoint))
        }
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
        } => Ok(DaemonReply::RuntimeSnapshot(Box::new(
            state
                .runtime_snapshot(&workspace_id, after_revision)
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
            session_id,
            command_id,
            command,
        } => {
            let history = if let RelayCommand::Prompt { prompt } = &command {
                let values = serde_json::to_value(prompt)?;
                let values = values
                    .as_array()
                    .context("serialized prompt content is not an array")?;
                let text = hel::hel_transcript::materialized_content_text(values);
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
            let session = state.session_manager.session(session_id).await?;
            let session_id = session.session_id().to_owned();
            let ordinal = session.submit(command_id, command).await?;
            if let Some((bundle_id, text)) = history
                && let Err(error) = blocking(move || {
                    hel::hel_database::record_prompt(&session_id, &bundle_id, ordinal, None, &text)
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
        DaemonAction::ForceStopSession { session_id } => {
            state.force_stop_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DestroyStoppedSession { session_id } => {
            state.destroy_stopped_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CancelLifecycle { session_id } => {
            state.cancel_lifecycle(&session_id)?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RecoverDraft { draft_id } => {
            blocking(move || hel::hel_database::recover_detached_draft(&draft_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Stop => {
            cancellation.cancel();
            Ok(DaemonReply::Done)
        }
    }
}

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

fn install_renamed_controller(state: &RuntimeState, controller: Controller) {
    *state
        .controller
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = controller;
    state.publish_revision();
}

fn workspace_snapshot(workspace_id: &str) -> Result<WorkspaceSnapshot> {
    let workspace = hel::hel_database::list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .with_context(|| format!("unknown workspace {workspace_id:?}"))?;
    let ids = hel::hel_database::session_ids_for_workspace(workspace_id)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let controller = Controller::load()?;
    let sessions = controller
        .state
        .sessions
        .values()
        .filter(|session| ids.contains(&session.id))
        .map(|session| SessionPreview {
            id: session.id.clone(),
            title: session.display_title().to_owned(),
            project: session.project_name(&controller.config),
            harness: session.harness_kind.display_name().to_owned(),
            state: session_state_label(session.state).to_owned(),
            active: session.state.is_active(),
            updated_at: session.updated_at.clone(),
        })
        .collect();
    let drafts = hel::hel_database::list_detached_drafts(workspace_id)?
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

fn session_state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Provisioning => "provisioning",
        SessionState::Running => "running",
        SessionState::Disconnected => "disconnected",
        SessionState::Checkpointing => "checkpointing",
        SessionState::Closing => "closing",
        SessionState::Destroying => "destroying",
        SessionState::Stopped => "stopped",
        SessionState::Lost => "lost",
        SessionState::Error => "error",
        SessionState::DestroyedWithDataLoss => "destroyed-with-data-loss",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graceful_close_retires_worker_polling_only_during_target_teardown() {
        assert!(!lifecycle_owns_worker_target(
            LifecycleKind::Close,
            Some(SessionState::Running)
        ));
        assert!(!lifecycle_owns_worker_target(
            LifecycleKind::Close,
            Some(SessionState::Checkpointing)
        ));
        assert!(!lifecycle_owns_worker_target(
            LifecycleKind::Close,
            Some(SessionState::Closing)
        ));
        assert!(lifecycle_owns_worker_target(
            LifecycleKind::Close,
            Some(SessionState::Destroying)
        ));
        assert!(lifecycle_owns_worker_target(
            LifecycleKind::ForceStop,
            Some(SessionState::Running)
        ));
    }

    /// A process that has exited but has not been reaped still answers
    /// `kill(pid, 0)`. The daemon-specific probe may reap its own child;
    /// platform process tables do not all expose a reliable Zombie status.
    #[cfg(unix)]
    #[test]
    fn a_process_that_exited_but_was_not_reaped_counts_as_gone() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn a process that exits immediately");
        let pid = child.id();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let gone = loop {
            if !daemon_process_is_alive(pid) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            gone,
            "an exited but unreaped process was reported as running"
        );
        // `daemon_process_is_alive` performed the waitpid reap. This explicit
        // wait is harmless (ECHILD on Unix) and documents that no Child is
        // abandoned.
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn attachment_liveness_probe_does_not_reap_children() {
        use std::io::Read;
        use std::process::Stdio;

        let mut child = std::process::Command::new("true")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn a process that exits immediately");
        let pid = child.id();
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("capture child stdout")
            .read_to_end(&mut output)
            .expect("observe child exit");

        let _ = process_is_alive(pid);
        let status = child.wait().expect("attachment probe left child waitable");
        assert!(status.success());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn zombie_only_daemon_group_counts_as_gone() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        let mut command = std::process::Command::new("true");
        command.process_group(0).stdout(Stdio::piped());
        let mut child = command
            .spawn()
            .expect("spawn process-group leader that exits immediately");
        let pid = libc::pid_t::try_from(child.id()).expect("child PID fits pid_t");
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("capture child stdout")
            .read_to_end(&mut output)
            .expect("observe child exit");

        assert!(!owned_daemon_group_is_alive(pid));
        child.wait().expect("reap process-group leader");
    }

    fn test_runtime_state() -> Arc<RuntimeState> {
        let remote = spawn_remote_session_manager().unwrap();
        let recovery =
            mj_controller::hel_recovery::RecoveryCoordinator::spawn(remote.control.clone());
        let upgrades = mj_controller::hel_worker_upgrade::WorkerUpgradeCoordinator::spawn(
            remote.control.clone(),
            &recovery.observer(),
        );
        Arc::new(RuntimeState::new_with_controller_loader(
            remote.control,
            Controller {
                config: HelConfig::default(),
                state: hel::hel_state::HelState::default(),
            },
            recovery.observer(),
            upgrades.observer(),
            Vec::new(),
            || {
                Ok(Controller {
                    config: HelConfig::default(),
                    state: hel::hel_state::HelState::default(),
                })
            },
        ))
    }

    #[tokio::test]
    async fn review_host_notifier_wakes_runtime_revision_subscribers() {
        let revisions = RuntimeRevisions::new(40);
        let mut subscriber = revisions.subscribe();
        // TurnReviewHost's behavior tests prove that view insert/change/remove
        // invokes this callback. This proves the production callback wired by
        // RuntimeState wakes the daemon and phone revision feed.
        let notify_review_publication = revisions.notifier();

        notify_review_publication();
        tokio::time::timeout(Duration::from_secs(1), subscriber.changed())
            .await
            .expect("review publication did not wake runtime subscribers")
            .expect("runtime revision publisher stopped");

        assert_eq!(*subscriber.borrow_and_update(), 41);
    }

    #[test]
    fn late_runtime_revision_publication_cannot_move_cursor_backwards() {
        let revisions = RuntimeRevisions::new(40);
        let subscriber = revisions.subscribe();

        revisions.publish_allocated(42);
        revisions.publish_allocated(41);

        assert_eq!(*subscriber.borrow(), 42);
    }

    #[tokio::test]
    async fn workspace_publication_reaches_existing_phone_subscriber() {
        let state = test_runtime_state();
        let mut workspaces = state.workspaces();
        let expected = WorkspaceRecord {
            id: "workspace-1".into(),
            name: "Reliability".into(),
            created_at: "2026-08-30T00:00:00Z".into(),
            last_opened_at: "2026-08-30T00:00:00Z".into(),
            session_count: 0,
        };

        state.publish_workspaces(vec![expected.clone()]);
        tokio::time::timeout(Duration::from_secs(1), workspaces.changed())
            .await
            .expect("workspace publication timed out")
            .expect("workspace publisher stopped");

        assert_eq!(workspaces.borrow_and_update().as_slice(), &[expected]);
    }

    #[tokio::test]
    async fn framing_round_trips_payloads_larger_than_a_pipe_buffer() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let sender = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            write_frame(&mut stream, &"x".repeat(512 * 1024))
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let received: String = read_frame(&mut stream).await.unwrap();
        sender.await.unwrap();
        assert_eq!(received.len(), 512 * 1024);
    }

    #[tokio::test]
    async fn framing_rejects_an_oversized_frame_before_allocating_it() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let sender = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_u32((MAX_FRAME_BYTES + 1) as u32)
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(read_frame::<String>(&mut stream).await.is_err());
        sender.await.unwrap();
    }

    /// A stopping daemon still holds a snapshot in memory, and used to serve
    /// it through the whole epilogue -- from a store it had stopped reading.
    /// Management stays answered so a client can still see it and stop it.
    #[tokio::test]
    async fn daemon_stops_serving_data_actions_once_shutdown_begins() {
        let state = test_runtime_state();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address,
            token: "right-token".into(),
            started_at: "now".into(),
            build_version: "test".into(),
        };
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let server_metadata = metadata.clone();
        let server_cancellation = cancellation.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, server_metadata, state, server_cancellation)
                .await
                .unwrap();
        });
        let mut stream = TcpStream::connect(address).await.unwrap();

        let mut request_id = 0;
        let mut ask = async |stream: &mut TcpStream, action: DaemonAction| {
            request_id += 1;
            write_frame(
                stream,
                &RequestEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    request_id,
                    token: "right-token".to_owned(),
                    action,
                },
            )
            .await
            .unwrap();
            read_frame::<ResponseEnvelope>(stream).await.unwrap().result
        };

        let refused = ask(
            &mut stream,
            DaemonAction::Snapshot {
                workspace_id: "workspace-a".into(),
            },
        )
        .await;
        assert_eq!(
            refused.unwrap_err(),
            "daemon is shutting down; retry to reach a fresh daemon"
        );
        assert!(matches!(
            ask(&mut stream, DaemonAction::Ping).await,
            Ok(DaemonReply::Pong)
        ));
        assert!(matches!(
            ask(&mut stream, DaemonAction::Status).await,
            Ok(DaemonReply::Status(_))
        ));
        assert!(matches!(
            ask(&mut stream, DaemonAction::Stop).await,
            Ok(DaemonReply::Done)
        ));

        drop(stream);
        server.await.unwrap();
    }

    /// The forced exit is the daemon's own bound, so it has to fire inside the
    /// window a client waiting on a stop is prepared to wait.
    #[test]
    fn shutdown_force_exit_finishes_before_the_stop_deadline() {
        assert!(SHUTDOWN_FORCE_EXIT_TIMEOUT < STOP_TIMEOUT);
    }

    #[tokio::test]
    async fn daemon_rejects_a_request_with_the_wrong_owner_token() {
        let state = test_runtime_state();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address,
            token: "right-token".into(),
            started_at: "now".into(),
            build_version: "test".into(),
        };
        let server_metadata = metadata.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, server_metadata, state, CancellationToken::new())
                .await
                .unwrap();
        });
        let mut stream = TcpStream::connect(address).await.unwrap();
        write_frame(
            &mut stream,
            &RequestEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request_id: 42,
                token: "wrong-token".into(),
                action: DaemonAction::Ping,
            },
        )
        .await
        .unwrap();
        let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
        assert_eq!(response.request_id, 42);
        assert_eq!(response.result.unwrap_err(), "daemon authentication failed");
        drop(stream);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_daemon_rejects_a_client_one_protocol_behind_before_dispatch() {
        assert_eq!(PROTOCOL_VERSION, 6);
        let state = test_runtime_state();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: 1,
            address,
            token: "right-token".into(),
            started_at: "now".into(),
            build_version: "test".into(),
        };
        let server_metadata = metadata.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_client(stream, server_metadata, state, CancellationToken::new())
                .await
                .unwrap();
        });
        let mut stream = TcpStream::connect(address).await.unwrap();
        write_frame(
            &mut stream,
            &RequestEnvelope {
                protocol_version: PROTOCOL_VERSION - 1,
                request_id: 43,
                token: metadata.token,
                action: DaemonAction::PersistReadReceipt {
                    client_id: "client-a".into(),
                    workspace_id: "workspace-a".into(),
                    session_id: "session-a".into(),
                    through: 7,
                },
            },
        )
        .await
        .unwrap();
        let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
        assert_eq!(response.request_id, 43);
        assert!(response.result.unwrap_err().contains(&format!(
            "incompatible daemon protocol {}; expected {PROTOCOL_VERSION}",
            PROTOCOL_VERSION - 1
        )));
        drop(stream);
        server.await.unwrap();
    }

    /// Pins the frozen management subset to its literal protocol-3 wire form.
    /// If this test fails, the change breaks cross-version daemon management;
    /// version the new behavior some other way.
    #[test]
    fn management_wire_shapes_stay_frozen_across_protocol_versions() {
        for (action, expected) in [
            (DaemonAction::Ping, serde_json::json!({"action": "ping"})),
            (
                DaemonAction::Status,
                serde_json::json!({"action": "status"}),
            ),
            (DaemonAction::Stop, serde_json::json!({"action": "stop"})),
        ] {
            let request = RequestEnvelope {
                protocol_version: 3,
                request_id: 7,
                token: "tok".into(),
                action,
            };
            assert_eq!(
                serde_json::to_value(&request).unwrap(),
                serde_json::json!({
                    "protocol_version": 3,
                    "request_id": 7,
                    "token": "tok",
                    "action": expected,
                })
            );
        }

        let response: ResponseEnvelope = serde_json::from_value(serde_json::json!({
            "protocol_version": 3,
            "request_id": 7,
            "result": {"Ok": {"reply": "status", "value": {
                "pid": 4242,
                "started_at": "2026-09-01T07:48:14Z",
                "build_version": "0.3.1",
                "attached_clients": 1,
                "phone_status": {"state": "disabled"},
            }}}
        }))
        .unwrap();
        match response.result.unwrap() {
            DaemonReply::Status(status) => {
                assert_eq!(status.pid, 4242);
                assert_eq!(status.build_version, "0.3.1");
            }
            reply => panic!("unexpected reply {reply:?}"),
        }
    }

    #[tokio::test]
    async fn daemon_serves_management_actions_for_any_protocol_version() {
        // 3 is an older shipped client; 5 stands in for a future one. Both
        // directions must stay manageable.
        for version in [3_u32, 5] {
            let state = test_runtime_state();
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let metadata = DaemonMetadata {
                protocol_version: PROTOCOL_VERSION,
                pid: 1,
                address,
                token: "right-token".into(),
                started_at: "now".into(),
                build_version: "test".into(),
            };
            let server_metadata = metadata.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                serve_client(stream, server_metadata, state, CancellationToken::new())
                    .await
                    .unwrap();
            });
            let mut stream = TcpStream::connect(address).await.unwrap();
            write_frame(
                &mut stream,
                &RequestEnvelope {
                    protocol_version: version,
                    request_id: 44,
                    token: "right-token".into(),
                    action: DaemonAction::Status,
                },
            )
            .await
            .unwrap();
            let response: ResponseEnvelope = read_frame(&mut stream).await.unwrap();
            assert_eq!(
                response.protocol_version, version,
                "reply must use the caller's dialect"
            );
            assert_eq!(response.request_id, 44);
            match response.result.unwrap() {
                DaemonReply::Status(status) => assert_eq!(status.build_version, "test"),
                reply => panic!("unexpected reply {reply:?}"),
            }
            drop(stream);
            server.await.unwrap();
        }
    }

    struct ProtocolTranscript {
        protocol_version: u32,
        daemon_build: &'static str,
        /// The exact frames the client must emit, in order (status, then stop).
        expected_requests: [serde_json::Value; 2],
        /// The exact frame bodies that version's daemon replies with, as raw
        /// JSON so the fixture cannot drift along with this build's types.
        responses: [&'static str; 2],
    }

    /// One transcript per released daemon protocol version, transcribed from
    /// the release tags (v0.3.x speaks 3, v0.4.x speaks 4; v0.1/v0.2 predate
    /// the daemon). When `PROTOCOL_VERSION` bumps, add the new version here —
    /// the frozen management subset means the entry differs only in its
    /// version number and build string. Do not edit existing entries: they are
    /// what shipped.
    fn released_protocol_transcripts() -> Vec<ProtocolTranscript> {
        let requests = |version: u32| {
            [
                serde_json::json!({
                    "protocol_version": version,
                    "request_id": 1,
                    "token": "tok",
                    "action": {"action": "status"},
                }),
                serde_json::json!({
                    "protocol_version": version,
                    "request_id": 2,
                    "token": "tok",
                    "action": {"action": "stop"},
                }),
            ]
        };
        vec![
            ProtocolTranscript {
                protocol_version: 3,
                daemon_build: "0.3.1",
                expected_requests: requests(3),
                responses: [
                    r#"{"protocol_version":3,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"0.3.1","attached_clients":1,"phone_status":{"state":"ready","viewer_url":"https://example.test:1","viewer_code":"690451","qr_login_url":null,"fallback_reason":null}}}}}"#,
                    r#"{"protocol_version":3,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
                ],
            },
            ProtocolTranscript {
                protocol_version: 4,
                daemon_build: "0.4.1",
                expected_requests: requests(4),
                responses: [
                    r#"{"protocol_version":4,"request_id":1,"result":{"Ok":{"reply":"status","value":{"pid":4242,"started_at":"2026-09-01T07:48:14Z","build_version":"0.4.1","attached_clients":1,"phone_status":{"state":"disabled"}}}}}"#,
                    r#"{"protocol_version":4,"request_id":2,"result":{"Ok":{"reply":"done"}}}"#,
                ],
            },
        ]
    }

    #[tokio::test]
    async fn management_client_talks_to_every_released_protocol_version() {
        for transcript in released_protocol_transcripts() {
            let protocol_version = transcript.protocol_version;
            let daemon_build = transcript.daemon_build;
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                for (expected, response) in transcript
                    .expected_requests
                    .iter()
                    .zip(transcript.responses)
                {
                    let request: serde_json::Value = read_frame(&mut stream).await.unwrap();
                    assert_eq!(
                        &request, expected,
                        "protocol {} daemon would reject this frame",
                        transcript.protocol_version
                    );
                    stream.write_u32(response.len() as u32).await.unwrap();
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.flush().await.unwrap();
                }
            });
            let metadata = DaemonMetadata {
                protocol_version,
                pid: 4242,
                address,
                token: "tok".into(),
                started_at: "2026-09-01T07:48:14Z".into(),
                build_version: daemon_build.into(),
            };
            let mut client = ManagementClient {
                inner: DaemonClient::connect(metadata).await.unwrap(),
            };
            let status = client.status().await.unwrap();
            assert_eq!(status.build_version, daemon_build);
            assert_eq!(status.attached_clients, 1);
            assert_eq!(client.protocol_version(), protocol_version);
            client.stop().await.unwrap();
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn equivalent_lifecycle_requests_join_one_daemon_operation() {
        let state = test_runtime_state();
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let first = state
            .start_or_join_lifecycle("session-1".into(), LifecycleKind::Close, {
                let starts = starts.clone();
                let release = release.clone();
                move |_state, _session_id, _cancelled| async move {
                    starts.fetch_add(1, Ordering::AcqRel);
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .unwrap();
        tokio::task::yield_now().await;
        let second = state
            .start_or_join_lifecycle(
                "session-1".into(),
                LifecycleKind::Close,
                |_state, _session_id, _cancelled| async move {
                    panic!("joined lifecycle request started duplicate work")
                },
            )
            .unwrap();
        assert_eq!(starts.load(Ordering::Acquire), 1);
        assert!(
            state
                .start_or_join_lifecycle(
                    "session-1".into(),
                    LifecycleKind::Resume,
                    |_state, _session_id, _cancelled| async move {
                        Ok(DaemonLifecycleResult::Done)
                    },
                )
                .is_err()
        );

        // The daemon task is independent of either client waiter.
        drop(first);
        release.notify_one();
        assert!(matches!(
            RuntimeState::wait_lifecycle_result(second).await.unwrap(),
            DaemonLifecycleResult::Done
        ));
        assert_eq!(starts.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn close_keeps_worker_target_available_for_checkpoint_lease() {
        let state = test_runtime_state();
        let release = Arc::new(tokio::sync::Notify::new());
        let result = state
            .start_or_join_lifecycle("session-1".into(), LifecycleKind::Close, {
                let release = release.clone();
                move |_state, _session_id, _cancelled| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .unwrap();

        assert!(
            state
                .worker_poll_exclusion_session_ids(
                    &state
                        .controller
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                )
                .is_empty()
        );

        release.notify_one();
        RuntimeState::wait_lifecycle_result(result).await.unwrap();
    }

    #[tokio::test]
    async fn daemon_lifecycle_reports_balanced_concurrent_stages() {
        struct UnusedExecutor;

        impl CommandExecutor for UnusedExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!("a stage notification must not run {}", command.program)
            }
        }

        let state = test_runtime_state();
        let release = Arc::new(tokio::sync::Notify::new());
        let result = state
            .start_or_join_lifecycle("session-1".into(), LifecycleKind::Create, {
                let release = release.clone();
                move |_state, _session_id, _cancelled| async move {
                    release.notified().await;
                    Ok(DaemonLifecycleResult::Done)
                }
            })
            .unwrap();
        assert_eq!(
            state.worker_poll_exclusion_session_ids(
                &state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            ),
            BTreeSet::from(["session-1".to_owned()])
        );
        let executor =
            DaemonStageReportingExecutor::new(UnusedExecutor, state.clone(), "session-1".into());
        executor.stage_started(ProvisionStage::Cloning);
        executor.stage_started(ProvisionStage::Cloning);
        executor.stage_started(ProvisionStage::Syncing);
        executor.stage_finished(ProvisionStage::Cloning);
        {
            let lifecycle = state
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let stages = &lifecycle.get("session-1").unwrap().active_stages;
            assert_eq!(stages.get(&ProvisionStage::Cloning).unwrap().0, 1);
            assert_eq!(stages.get(&ProvisionStage::Syncing).unwrap().0, 1);
        }
        executor.stage_finished(ProvisionStage::Cloning);
        executor.stage_finished(ProvisionStage::Syncing);
        assert!(
            state
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get("session-1")
                .unwrap()
                .active_stages
                .is_empty()
        );
        release.notify_one();
        assert!(matches!(
            RuntimeState::wait_lifecycle_result(result).await.unwrap(),
            DaemonLifecycleResult::Done
        ));
    }
}
