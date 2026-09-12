//! Authenticated local daemon protocol and client transport.
use crate::review::RuntimeReviewView;
use crate::session::{ManagedSessionView, ViewError};
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::{Config, data_dir};
use mj_core::credentials::CredentialSyncSignal;
use mj_core::elicitation::ElicitationResponse;
use mj_core::relay::{RelayCommand, RelayOperationalState};
use mj_core::review::driver::Resolution;
use mj_core::state::*;
use mj_core::targets::{AdditionalMount, ProvisionStage};
use mj_core::workspace::WorkspaceRecord;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
pub fn metadata_path() -> PathBuf {
    data_dir().join("daemon.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonMetadata {
    pub protocol_version: u32,
    pub pid: u32,
    pub address: SocketAddr,
    pub token: String,
    pub started_at: String,
    pub build_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceListing {
    pub workspace: WorkspaceRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPreview {
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
pub struct WorkspaceSnapshot {
    pub workspace: WorkspaceRecord,
    pub sessions: Vec<SessionPreview>,
    pub drafts: Vec<DraftPreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionView {
    pub session_id: String,
    pub projection_ordinal: u64,
    pub projection_digest: String,
    pub operational: Option<RelayOperationalState>,
    pub latest_credential_sync_signal: Option<CredentialSyncSignal>,
    pub connected: bool,
    pub error: Option<ViewError>,
}

impl RuntimeSessionView {
    pub fn from_managed(session_id: String, view: ManagedSessionView) -> Self {
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
pub struct RuntimeNotice {
    pub id: u64,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub workspace_names: BTreeMap<String, String>,
    #[serde(default)]
    pub moves: Vec<mj_core::state::MoveOperation>,
    pub revision: u64,
    pub config: Config,
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
pub enum RuntimeLifecycleKind {
    Create,
    Close,
    Resume,
    Move,
    ForceStop,
    DestroyStopped,
    ForceDestroy,
    Cleanup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLifecycleView {
    pub operation_id: String,
    pub cancellable: bool,
    pub session_id: String,
    pub kind: RuntimeLifecycleKind,
    pub started_at_epoch_seconds: u64,
    pub active_stages: Vec<(ProvisionStage, u64)>,
    pub resume_destination: Option<(String, String)>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeSessionRequest {
    pub session_id: String,
    pub workspace_id: String,
    pub profile_id: String,
    pub target_template_id: String,
    pub additional_mounts: Option<Vec<AdditionalMount>>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub discard_queue: bool,
    pub repository_preflight: Option<ResumeRepositorySourceReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub initial_prompt: Option<String>,
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
pub struct RegisteredSession {
    pub session: SessionRecord,
    pub remembered_container_size: Option<(String, HostContainerSize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftPreview {
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
pub enum DaemonAction {
    Ping,
    Status,
    WebViewerAccess,
    RecoverWebViewer(crate::web::WebViewerRecovery),
    InspectWebListener,
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
    },
    TouchWorkspace {
        workspace_id: String,
    },
    DeleteWorkspace {
        workspace_id: String,
    },
    Attach {
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
        draft: mj_core::storage::DetachedSessionDraft,
    },
    SaveActiveReview {
        session_id: String,
        review: mj_core::storage::StoredReview,
    },
    ClearActiveReview {
        session_id: String,
    },
    RememberReviewerSelection {
        workspace_id: String,
        selection: mj_core::second_opinion::ReviewerSelection,
    },
    SaveWorkspacePaneSizes {
        workspace_id: String,
        sizes: mj_core::workspace::PaneSizes,
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
        #[serde(default)]
        all_workspaces: bool,
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
        #[serde(default)]
        inherited_draft: Option<String>,
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
    StopBackgroundTask {
        session_id: String,
        background_task_id: String,
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
        action: crate::session::ReviewerAction,
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
    PrepareMoveSession(MoveSelection),
    MoveSession(MoveSessionRequest),
    ForceStopSession {
        session_id: String,
    },
    DestroyStoppedSession {
        session_id: String,
    },
    ForceDestroySession {
        session_id: String,
    },
    ForceDeleteWorkspace {
        workspace_id: String,
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
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: u64,
    pub token: String,
    pub action: DaemonAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: u64,
    pub result: std::result::Result<DaemonReply, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reply", content = "value")]
pub enum DaemonReply {
    Pong,
    Status(DaemonStatus),
    WebViewerAccess(crate::web::WebViewerAccess),
    WebListeners(Vec<crate::web::WebListenerProcess>),
    Workspaces(Vec<WorkspaceListing>),
    Workspace(WorkspaceRecord),
    Snapshot(WorkspaceSnapshot),
    RuntimeSnapshot(Box<RuntimeSnapshot>),
    RegisteredSession(Box<RegisteredSession>),
    MovePreparation(Box<MovePreparation>),
    MoveOutcome(MoveOutcome),
    Ordinal(u64),
    Text(String),
    OptionalSessionState(Option<SessionState>),
    Checkpoint(mj_core::state::CheckpointMetadata),
    RecoveryScan(mj_core::state::RecoveryScan),
    Reviewer(Box<crate::session::ReviewerOutcome>),
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at: String,
    pub build_version: String,
    pub attached_clients: usize,
    pub phone_status: WebViewerStatus,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum WebViewerStatus {
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
pub fn process_is_zombie(pid: u32) -> bool {
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
pub async fn wait_for_exit(pid: u32) -> Result<()> {
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
pub fn daemon_process_is_alive(pid: u32) -> bool {
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
pub fn owned_daemon_group_is_alive(pid: libc::pid_t) -> bool {
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

pub fn process_is_alive(pid: u32) -> bool {
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

pub fn read_metadata() -> Result<DaemonMetadata> {
    let metadata = read_metadata_any()?;
    ensure!(
        metadata.protocol_version == PROTOCOL_VERSION,
        "daemon protocol {} is incompatible with client protocol {}",
        metadata.protocol_version,
        PROTOCOL_VERSION
    );
    Ok(metadata)
}

pub fn read_metadata_any() -> Result<DaemonMetadata> {
    let path = metadata_path();
    let body = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let metadata: DaemonMetadata =
        serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))?;
    Ok(metadata)
}

pub async fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    ensure!(body.len() <= MAX_FRAME_BYTES, "daemon frame is too large");
    stream.write_u32(body.len() as u32).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    ensure!(
        length <= MAX_FRAME_BYTES,
        "daemon frame exceeds {MAX_FRAME_BYTES} bytes"
    );
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("decode daemon frame")
}

pub struct DaemonClient {
    metadata: DaemonMetadata,
    stream: TcpStream,
    next_request_id: u64,
}

impl DaemonClient {
    pub async fn connect(metadata: DaemonMetadata) -> Result<Self> {
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
    pub async fn request(&mut self, action: DaemonAction) -> Result<DaemonReply> {
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

    pub async fn status(&mut self) -> Result<DaemonStatus> {
        match self.request(DaemonAction::Status).await? {
            DaemonReply::Status(status) => Ok(status),
            reply => bail!("unexpected daemon status reply {reply:?}"),
        }
    }

    pub async fn web_access(&mut self) -> Result<crate::web::WebViewerAccess> {
        match self.request(DaemonAction::WebViewerAccess).await? {
            DaemonReply::WebViewerAccess(access) => Ok(access),
            reply => bail!("unexpected web viewer reply {reply:?}"),
        }
    }

    pub async fn recover_web_viewer(
        &mut self,
        action: crate::web::WebViewerRecovery,
    ) -> Result<()> {
        match self.request(DaemonAction::RecoverWebViewer(action)).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected web viewer recovery reply {reply:?}"),
        }
    }

    pub async fn inspect_web_listener(&mut self) -> Result<Vec<crate::web::WebListenerProcess>> {
        match self.request(DaemonAction::InspectWebListener).await? {
            DaemonReply::WebListeners(processes) => Ok(processes),
            reply => bail!("unexpected listener inspection reply {reply:?}"),
        }
    }

    pub async fn list_workspaces(&mut self) -> Result<Vec<WorkspaceListing>> {
        match self.request(DaemonAction::ListWorkspaces).await? {
            DaemonReply::Workspaces(workspaces) => Ok(workspaces),
            reply => bail!("unexpected daemon workspace reply {reply:?}"),
        }
    }

    pub async fn rename_profile(&mut self, old_id: String, new_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameProfile { old_id, new_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-profile reply {reply:?}"),
        }
    }

    pub async fn rename_target(&mut self, old_id: String, new_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameTarget { old_id, new_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-target reply {reply:?}"),
        }
    }

    pub async fn create_workspace(&mut self, name: String) -> Result<WorkspaceRecord> {
        match self.request(DaemonAction::CreateWorkspace { name }).await? {
            DaemonReply::Workspace(workspace) => Ok(workspace),
            reply => bail!("unexpected create-workspace reply {reply:?}"),
        }
    }

    pub async fn rename_workspace(&mut self, workspace_id: String, name: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameWorkspace { workspace_id, name })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-workspace reply {reply:?}"),
        }
    }

    pub async fn touch_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::TouchWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected touch-workspace reply {reply:?}"),
        }
    }

    pub async fn delete_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::DeleteWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected delete-workspace reply {reply:?}"),
        }
    }

    pub async fn attach(&mut self, client_id: String, pid: u32) -> Result<()> {
        match self
            .request(DaemonAction::Attach { client_id, pid })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected attach reply {reply:?}"),
        }
    }

    pub async fn detach(&mut self, client_id: String) -> Result<()> {
        match self.request(DaemonAction::Detach { client_id }).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected detach reply {reply:?}"),
        }
    }

    pub async fn persist_read_receipt(
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

    pub async fn persist_detached_session_state(
        &mut self,
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
        owner_pid: u32,
        draft: mj_core::storage::DetachedSessionDraft,
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

    pub async fn save_active_review(
        &mut self,
        session_id: String,
        review: mj_core::storage::StoredReview,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SaveActiveReview { session_id, review })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected save-review reply {reply:?}"),
        }
    }

    pub async fn clear_active_review(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ClearActiveReview { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected clear-review reply {reply:?}"),
        }
    }

    pub async fn remember_reviewer_selection(
        &mut self,
        workspace_id: String,
        selection: mj_core::second_opinion::ReviewerSelection,
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

    pub async fn save_workspace_pane_sizes(
        &mut self,
        workspace_id: String,
        sizes: mj_core::workspace::PaneSizes,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SaveWorkspacePaneSizes {
                workspace_id,
                sizes,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected pane-size save reply {reply:?}"),
        }
    }

    pub async fn persist_imported_session(&mut self, session: SessionRecord) -> Result<()> {
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

    pub async fn set_session_title(&mut self, session_id: String, title: String) -> Result<String> {
        match self
            .request(DaemonAction::SetSessionTitle { session_id, title })
            .await?
        {
            DaemonReply::Text(title) => Ok(title),
            reply => bail!("unexpected session-title reply {reply:?}"),
        }
    }

    pub async fn set_session_container_settings(
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

    pub async fn set_session_acp_title(
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

    pub async fn mark_session_target_missing(
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

    pub async fn checkpoint_session(
        &mut self,
        session_id: String,
    ) -> Result<mj_core::state::CheckpointMetadata> {
        match self
            .request(DaemonAction::CheckpointSession { session_id })
            .await?
        {
            DaemonReply::Checkpoint(checkpoint) => Ok(checkpoint),
            reply => bail!("unexpected checkpoint reply {reply:?}"),
        }
    }

    pub async fn scan_recovery(&mut self) -> Result<mj_core::state::RecoveryScan> {
        match self.request(DaemonAction::ScanRecovery).await? {
            DaemonReply::RecoveryScan(scan) => Ok(scan),
            reply => bail!("unexpected recovery-scan reply {reply:?}"),
        }
    }

    pub async fn adopt_recovery(
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

    pub async fn destroy_recovery(
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

    pub async fn snapshot(&mut self, workspace_id: String) -> Result<WorkspaceSnapshot> {
        match self
            .request(DaemonAction::Snapshot { workspace_id })
            .await?
        {
            DaemonReply::Snapshot(snapshot) => Ok(snapshot),
            reply => bail!("unexpected snapshot reply {reply:?}"),
        }
    }

    pub async fn runtime_snapshot(
        &mut self,
        workspace_id: String,
        after_revision: u64,
        all_workspaces: bool,
    ) -> Result<RuntimeSnapshot> {
        match self
            .request(DaemonAction::RuntimeSnapshot {
                workspace_id,
                after_revision,
                all_workspaces,
            })
            .await?
        {
            DaemonReply::RuntimeSnapshot(snapshot) => Ok(*snapshot),
            reply => bail!("unexpected runtime snapshot reply {reply:?}"),
        }
    }

    pub async fn submit_session_command(
        &mut self,
        session_id: String,
        command_id: String,
        command: RelayCommand,
        inherited_draft: Option<String>,
    ) -> Result<u64> {
        match self
            .request(DaemonAction::SubmitSessionCommand {
                inherited_draft,
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
    pub async fn start_turn_review(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::StartTurnReview { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected turn-review reply {reply:?}"),
        }
    }

    pub async fn resolve_turn_review(
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

    pub async fn reviewer_action(
        &mut self,
        session_id: String,
        role: Option<String>,
        action: crate::session::ReviewerAction,
    ) -> Result<crate::session::ReviewerOutcome> {
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

    pub async fn sync_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::SyncSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected session sync reply {reply:?}"),
        }
    }

    pub async fn respond_elicitation(
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

    pub async fn stop_background_task(
        &mut self,
        session_id: String,
        background_task_id: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::StopBackgroundTask {
                session_id,
                background_task_id,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected background task stop reply {reply:?}"),
        }
    }

    pub async fn close_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CloseSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected close-session reply {reply:?}"),
        }
    }

    pub async fn start_create_session(
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

    pub async fn wait_create_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::WaitCreateSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected wait-create reply {reply:?}"),
        }
    }

    pub async fn resume_session(&mut self, request: ResumeSessionRequest) -> Result<()> {
        match self.request(DaemonAction::ResumeSession(request)).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected resume-session reply {reply:?}"),
        }
    }

    pub async fn force_stop_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ForceStopSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected force-stop reply {reply:?}"),
        }
    }

    pub async fn destroy_stopped_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::DestroyStoppedSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected destroy-stopped reply {reply:?}"),
        }
    }

    pub async fn force_destroy_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ForceDestroySession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected force-destroy reply {reply:?}"),
        }
    }

    pub async fn force_delete_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ForceDeleteWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected force-delete-workspace reply {reply:?}"),
        }
    }

    pub async fn cancel_lifecycle(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CancelLifecycle { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected cancel-lifecycle reply {reply:?}"),
        }
    }

    pub async fn recover_draft(&mut self, draft_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RecoverDraft { draft_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected recover-draft reply {reply:?}"),
        }
    }

    pub async fn stop(&mut self) -> Result<()> {
        match self.request(DaemonAction::Stop).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected stop reply {reply:?}"),
        }
    }
}

pub async fn connect_existing() -> Result<DaemonClient> {
    let metadata = tokio::task::spawn_blocking(read_metadata)
        .await
        .context("read daemon metadata task failed")??;
    DaemonClient::connect(metadata).await
}

/// A handle to whatever daemon the metadata file advertises, regardless of its
/// protocol version. It only exposes the frozen management subset (`Ping`,
/// `Status`, `Stop`), which encodes identically in every protocol version.
pub struct ManagementClient {
    inner: DaemonClient,
}

impl ManagementClient {
    pub fn new(inner: DaemonClient) -> Self {
        Self { inner }
    }
    pub fn protocol_version(&self) -> u32 {
        self.inner.metadata.protocol_version
    }

    pub async fn status(&mut self) -> Result<DaemonStatus> {
        self.inner.status().await
    }

    pub async fn stop(&mut self) -> Result<()> {
        tokio::time::timeout(STOP_TIMEOUT, self.inner.stop())
            .await
            .context("Mjolnir daemon did not acknowledge the stop before the deadline")?
    }

    /// Ask the daemon to stop and wait for its process to actually exit.
    pub async fn stop_and_wait(mut self) -> Result<()> {
        let pid = self.inner.metadata.pid;
        self.stop().await?;
        wait_for_exit(pid).await.with_context(|| {
            format!(
                "Mjolnir daemon {pid} accepted the stop but was still running after {}s",
                STOP_TIMEOUT.as_secs()
            )
        })
    }
}

pub async fn connect_management() -> Result<ManagementClient> {
    Ok(ManagementClient {
        inner: DaemonClient::connect(read_metadata_any()?).await?,
    })
}

pub fn ensure_supported_daemon_protocol(version: u32) -> Result<()> {
    ensure!(
        version <= PROTOCOL_VERSION,
        "the daemon uses a newer protocol ({version}) than this client ({PROTOCOL_VERSION}); restart this client with the updated mj binary"
    );
    Ok(())
}
pub const PROTOCOL_VERSION: u32 = 18;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// How long a daemon is given to exit after it accepts a stop.
///
/// Stopping cancels a token and returns immediately; the daemon then unwinds
/// its session manager, its phone server and its pollers. That is normally
/// fast, but a daemon whose database has been migrated out from under it fails
/// every read while it winds down and has been observed taking over five
/// seconds — which the previous five-second bound missed by a fraction,
/// reporting a stop that had in fact worked as `did not stop` and aborting the
/// restart that depended on it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);
pub const RETRY_DELAY: Duration = Duration::from_millis(40);
impl DaemonClient {
    pub async fn prepare_move_session(
        &mut self,
        selection: MoveSelection,
    ) -> Result<MovePreparation> {
        match self
            .request(DaemonAction::PrepareMoveSession(selection))
            .await?
        {
            DaemonReply::MovePreparation(preparation) => Ok(*preparation),
            _ => bail!("daemon returned an unexpected move preparation reply"),
        }
    }

    pub async fn move_session(&mut self, request: MoveSessionRequest) -> Result<MoveOutcome> {
        match self.request(DaemonAction::MoveSession(request)).await? {
            DaemonReply::MoveOutcome(outcome) => Ok(outcome),
            _ => bail!("daemon returned an unexpected move reply"),
        }
    }
}
