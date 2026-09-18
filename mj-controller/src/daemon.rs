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
use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use anyhow::{Context, Result, anyhow, bail, ensure};
use mj_core::config::Config;
use mj_core::refusal::Refusal;
use mj_core::relay::RelayCommand;
use mj_core::state::{RecoveryObservation, SessionRecord, SessionState};
use mj_core::subagent::SubagentRecord;

use crate::controller::{
    BranchDisposition, Controller, ControllerStoreGuard, SessionLaunchOptions, SessionResumeOptions,
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
    unowned_interrupted_lifecycles,
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
    /// Work waiting for one session's harness to become ready: the prompts a
    /// person typed while it started, and the hand-off a restored session
    /// carries. One ordered queue per session, each drained by one task.
    startup_prompts: Mutex<BTreeMap<String, StartupQueue>>,
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
    /// Publishes checkpointed sessions into the user's SessionWiki index.
    wiki: crate::sessionwiki::WikiIndexer,
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
    /// The archive job's destruction: the same teardown as `DestroyStopped`,
    /// with the session's git branch kept unless another branch already
    /// contains every one of its commits. Surfaces see it as a destroy.
    ArchiveStopped,
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
    /// Nothing to checkpoint: tear down whatever target is left and settle.
    SettleWithoutCheckpoint,
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
    } else if crate::controller::has_nothing_to_checkpoint(session) {
        CloseRoute::SettleWithoutCheckpoint
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

/// One piece of work that waits for a starting session's harness.
///
/// Both kinds are ordered against each other on purpose: a restored session's
/// hand-off is the hidden context its first prompt reads, so it has to be
/// installed before any queued prompt is submitted.
pub(crate) enum StartupStep {
    InstallHandoff(Box<mj_core::archive::CanonicalSessionSnapshot>),
    Prompt {
        text: String,
        inherited_draft: Option<String>,
    },
}

/// The steps waiting for one session, and the task draining them.
///
/// The entry exists only while a drain task owns it. `in_flight` marks a step
/// that has been popped and is running, so the queue is never treated as empty
/// while its last step is still being carried out.
struct StartupQueue {
    pending: VecDeque<StartupStep>,
    in_flight: bool,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
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
    result: LifecycleWatch,
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

/// How one lifecycle operation ended when it failed.
///
/// The result is broadcast to every waiter, which is why it cannot simply be
/// the `anyhow::Error`: that is not clonable. Keeping the refusal beside the
/// text is what lets a reason written for the caller survive the crossing; a
/// failure rebuilt from a string alone would arrive as an internal fault.
#[derive(Debug, Clone)]
pub(crate) struct LifecycleFailure {
    detail: String,
    refusal: Option<Refusal>,
}

/// One lifecycle operation's outcome, and the channel every waiter reads it
/// from. `None` means the operation is still running.
type LifecycleResult = std::result::Result<DaemonLifecycleResult, LifecycleFailure>;
type LifecycleWatch = tokio::sync::watch::Receiver<Option<LifecycleResult>>;

impl LifecycleFailure {
    fn of(error: &anyhow::Error) -> Self {
        Self {
            detail: format!("{error:#}"),
            refusal: Refusal::of(error),
        }
    }

    /// A failure with no reason written for a caller, such as a task that died
    /// before the operation could say anything about itself.
    fn internal(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            refusal: None,
        }
    }

    /// Rebuild the error a waiter sees, with the refusal still attached.
    fn into_error(self) -> anyhow::Error {
        match self.refusal {
            Some(refusal) => anyhow::Error::new(refusal).context(self.detail),
            None => anyhow::Error::msg(self.detail),
        }
    }
}

impl std::fmt::Display for LifecycleFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl From<LifecycleKind> for RuntimeLifecycleKind {
    fn from(kind: LifecycleKind) -> Self {
        match kind {
            LifecycleKind::Create => Self::Create,
            LifecycleKind::Close => Self::Close,
            LifecycleKind::Resume => Self::Resume,
            LifecycleKind::Move => Self::Move,
            LifecycleKind::ForceStop => Self::ForceStop,
            LifecycleKind::DestroyStopped | LifecycleKind::ArchiveStopped => Self::DestroyStopped,
            LifecycleKind::ForceDestroy => Self::ForceDestroy,
            LifecycleKind::Cleanup => Self::Cleanup,
        }
    }
}

mod close;
mod create;
mod lifecycle;
mod resume;
mod snapshot;
mod state;
mod support;
mod views;
use support::*;
mod process;
pub use process::*;
mod serve;
use serve::*;
mod actions;
use actions::*;
mod guards;
pub(crate) use guards::*;

#[cfg(test)]
mod tests;
