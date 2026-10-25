//! Persistent per-user controller daemon and its authenticated local protocol.

mod container_upgrade;
mod feed;
mod owner;
mod record_index;
mod restart;
mod session_move;
use crate::controller::move_session::{
    MoveMutationGuard, MoveOutcome, MovePreparation, MoveSelection, MoveSessionRequest,
};
pub use mj_client::daemon::*;
use owner::RuntimeStateOwner;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
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

use crate::controller::{
    BeforeClose, BranchDisposition, CheckoutDisposition, Controller, ControllerStoreGuard,
    SessionLaunchOptions, SessionResumeOptions,
};
use crate::review_host::TurnReviewHost;
use crate::session_manager::{
    ManagedSessionView, RemoteSessionPublisher, RemoteSessionRequest, SessionManagerChannels,
    SessionManagerControl, ViewError, new_command_id, spawn_remote_session_manager,
};
#[cfg(test)]
use crate::session_manager::{RelaySessionTarget, RemoteSessionRequests, SessionManagerShutdown};
use crate::worker_upgrade::{WorkerUpgradeObservation, WorkerUpgradeObserver};
use mj_core::workspace::WorkspaceRecord;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::pollers::{
    dashboard_worker_targets, interrupted_destroy_session_ids, interrupted_suspend_session_ids,
    reserve_recovery_or_cancel, spawn_image_refresher, unowned_interrupted_lifecycles,
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

/// Whether a running lifecycle operation can still be cancelled: open, then
/// either cancelled or committed, and exactly one of those two wins. A cancel
/// records whether daemon shutdown made it.
///
/// An operation commits at the first step a cancel could no longer undo: a
/// create once its new session is published, a graceful close once its
/// verified checkpoint is installed and it starts stopping the session. This
/// is the one owner of whether the operation itself can still be cancelled,
/// and the operation's executor reads [`Self::cancelled`], which is set only
/// when a cancel is accepted, so a committed operation never sees its
/// commands fail as cancelled (#1218). Daemon shutdown is the one exception
/// ([`Self::interrupt`]).
#[derive(Clone, Default)]
pub struct LifecycleControl {
    state: Arc<AtomicU8>,
    pub cancelled: Arc<AtomicBool>,
}

const LIFECYCLE_OPEN: u8 = 0;
const LIFECYCLE_CANCELLED: u8 = 1;
const LIFECYCLE_COMMITTED: u8 = 2;
/// Cancelled because the daemon is stopping, not by a person or a close.
const LIFECYCLE_CANCELLED_FOR_SHUTDOWN: u8 = 3;

impl LifecycleControl {
    pub fn request_cancel(&self) -> bool {
        self.cancel_as(LIFECYCLE_CANCELLED)
    }

    fn cancel_for_shutdown(&self) -> bool {
        self.cancel_as(LIFECYCLE_CANCELLED_FOR_SHUTDOWN)
    }

    /// The state is written before the flag the executor reads, so an
    /// operation that sees its cancel also sees which cancel it was.
    fn cancel_as(&self, cancelled: u8) -> bool {
        let accepted = self
            .state
            .compare_exchange(
                LIFECYCLE_OPEN,
                cancelled,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if accepted {
            self.cancelled.store(true, Ordering::Release);
        }
        accepted
    }

    /// Commit the operation, unless a cancel was accepted first. Only the
    /// first commit is granted.
    pub fn grant_commit(&self) -> bool {
        self.state
            .compare_exchange(
                LIFECYCLE_OPEN,
                LIFECYCLE_COMMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn is_cancellable(&self) -> bool {
        self.state.load(Ordering::Acquire) == LIFECYCLE_OPEN
    }

    fn is_committed(&self) -> bool {
        self.state.load(Ordering::Acquire) == LIFECYCLE_COMMITTED
    }

    fn cancel_accepted(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            LIFECYCLE_CANCELLED | LIFECYCLE_CANCELLED_FOR_SHUTDOWN
        )
    }

    /// Stop the operation's commands because the daemon is exiting, whether
    /// or not it committed. This is not a cancel: the operation fails where it
    /// is, and the next daemon finishes it from its durable record.
    fn interrupt(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Whether daemon shutdown is what stopped the operation's commands, so a
    /// failure can say so instead of only that an operation was cancelled
    /// (#1321). A committed operation's commands are stopped only by
    /// [`Self::interrupt`].
    pub fn stopped_for_shutdown(&self) -> bool {
        match self.state.load(Ordering::Acquire) {
            LIFECYCLE_CANCELLED_FOR_SHUTDOWN => true,
            LIFECYCLE_COMMITTED => self.cancelled.load(Ordering::Acquire),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct Attachment {
    pid: u32,
}

#[derive(Default)]
struct QuotaBoard {
    snapshot: mj_client::quota::QuotaSnapshot,
    refresh: Option<tokio::sync::mpsc::Sender<()>>,
}

pub struct RuntimeState {
    attachments: Mutex<BTreeMap<String, Attachment>>,
    phone_status: Mutex<WebViewerStatus>,
    pub web_viewer: crate::web_viewer::ViewerControl,
    ever_attached: AtomicBool,
    revisions: RuntimeRevisions,
    workspaces_tx: tokio::sync::watch::Sender<Vec<WorkspaceRecord>>,
    workspace_refresh: tokio::sync::Mutex<()>,
    session_manager: SessionManagerControl,
    owner: Mutex<RuntimeStateOwner>,
    /// Rebuilt by the delegation service at daemon startup; weak so the
    /// backend's ExportRuntime reference does not form an Arc cycle.
    pub(crate) report_backend:
        std::sync::OnceLock<std::sync::Weak<crate::server_runtime::api::ApiBackend>>,
    pub(crate) credential_targets:
        Arc<tokio::sync::watch::Sender<Vec<mj_core::credentials::CredentialSyncTarget>>>,
    feed: Mutex<feed::RuntimeHistory>,
    recent_launch: tokio::sync::watch::Sender<Option<mj_core::go::RecentLaunch>>,
    recent_launch_update: Mutex<()>,
    committed: Option<
        tokio::sync::watch::Receiver<
            std::result::Result<Arc<crate::database::CommittedState>, Arc<str>>,
        >,
    >,
    workspace_closes: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    /// Resume ownership can precede its durable workspace assignment.
    workspace_resume_admission: Mutex<BTreeMap<String, Arc<tokio::sync::RwLock<()>>>>,
    /// The bounded wait for each live session's harness to become usable.
    /// Driven only by the daemon's background readiness sweep.
    harness_readiness: Mutex<HarnessReadinessWatch>,
    /// Work waiting for one session's harness to become ready: the prompts a
    /// person typed while it started, and the hand-off a restored session
    /// carries. One ordered queue per session, each drained by one task.
    startup_prompts: Mutex<BTreeMap<String, StartupQueue>>,
    startup_enqueue: tokio::sync::Mutex<()>,
    controller_loader: fn() -> Result<Controller>,
    config_mutation: tokio::sync::Mutex<()>,
    projects: Arc<crate::project_catalog::Catalog>,
    profile_catalog: crate::review_host::SharedProfileCatalog,
    recovery_observer: RecoveryObserver,
    worker_upgrade_observer: WorkerUpgradeObserver,
    /// Recent background notices, newest last, with the id of the next one.
    /// Bounded: a surface that never attaches must not make this grow.
    notices: Mutex<VecDeque<RuntimeNotice>>,
    next_notice_id: AtomicU64,
    /// What the quota poller last published, and how to wake it. The poller
    /// is the only process that asks a provider for quota; everything else
    /// reads this.
    quota: Mutex<QuotaBoard>,
    /// The one capacity poller and what it shares; set once the daemon
    /// starts it.
    capacity: std::sync::OnceLock<capacity::CapacityFeed>,
    /// Serializes a new container reservation with the durable creation of the
    /// provisioning session that represents it.
    container_admission: tokio::sync::Mutex<()>,
    /// What `[review]` last said, republished by the target refresher.
    review_config: Arc<Mutex<mj_core::config::ReviewConfig>>,
    /// Turn review runs here, in the process that owns every session, so a
    /// review happens whether the terminal, the phone, or nobody is attached.
    review_host: TurnReviewHost,
    reviewer_runtime: crate::review_host::SharedReviewerRuntime,
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
    Suspend,
    Resume,
    Restart,
    UpgradeContainer,
    Move,
    ForceStop,
    DestroyStopped,
    /// The archive job's destruction: the same teardown as `DestroyStopped`,
    /// with the session's git branch kept unless another branch already
    /// contains every one of its commits. Surfaces see it as a destroy.
    ArchiveStopped,
    ForceDestroy,
    /// A sub-agent stopped because its parent is being suspended: the
    /// teardown of `ForceDestroy`, owned by the parent's suspend, so it is
    /// shown as a stop and cannot be cancelled on its own.
    StopSubagent,
    /// A sub-agent whose turn ended and whose parent was told is having its
    /// worker stopped, keeping everything else (#1161).
    Park,
    /// A parked sub-agent's worker is being started again for its parent's
    /// `send_message`.
    Unpark,
    Cleanup,
    StartupCleanup,
}

impl LifecycleKind {
    /// Whether this operation is a destroy's teardown of the session, which a
    /// second destroy of the same session waits for instead of cancelling.
    fn is_teardown(self) -> bool {
        matches!(
            self,
            LifecycleKind::ForceDestroy
                | LifecycleKind::DestroyStopped
                | LifecycleKind::ArchiveStopped
                | LifecycleKind::StopSubagent
        )
    }

    /// Whether only the operation that started this one may cancel it, never a
    /// person directly. A sub-agent's stop belongs to its parent's suspend,
    /// which is what a person cancels. A park and an unpark belong to the
    /// parent's sub-agent tools: a person ends either by closing the child,
    /// whose close cancels or waits for them.
    fn cancelled_only_by_its_owner(self) -> bool {
        matches!(
            self,
            LifecycleKind::StopSubagent | LifecycleKind::Park | LifecycleKind::Unpark
        )
    }

    /// What a refusal calls this operation, so a person told that a session is
    /// busy learns which operation is holding it (#1010).
    fn label(self) -> &'static str {
        match self {
            LifecycleKind::Create => "create",
            LifecycleKind::Suspend => "suspend",
            LifecycleKind::Resume => "resume",
            LifecycleKind::Restart => "restart",
            LifecycleKind::UpgradeContainer => "container upgrade",
            LifecycleKind::Move => "move",
            LifecycleKind::ForceStop => "force stop",
            LifecycleKind::DestroyStopped => "destroy",
            LifecycleKind::ArchiveStopped => "archive",
            LifecycleKind::ForceDestroy => "force destroy",
            LifecycleKind::StopSubagent => "stop",
            LifecycleKind::Park => "park",
            LifecycleKind::Unpark => "restart",
            LifecycleKind::Cleanup => "cleanup",
            LifecycleKind::StartupCleanup => "failed startup cleanup",
        }
    }
}

/// Whether a lifecycle has exclusive ownership of the worker target, so the
/// session manager must stop polling it. A graceful close needs the manager's
/// relay lease through checkpointing and sealing; once the durable state says
/// `Destroying`, that lease has been released and target teardown is exclusive.
///
/// A park needs the session actor: it takes the actor's connection to admit
/// the park only while the worker is idle. Once the record says `Parked` the
/// manager drops the session on its own. Once unpark commits an active record
/// and boot intent, the actor can observe startup; WorkerPermit still prevents
/// it from racing a process mutation by the wake owner.
fn lifecycle_owns_worker_target(kind: LifecycleKind, state: Option<SessionState>) -> bool {
    match kind {
        LifecycleKind::Suspend => state == Some(SessionState::Destroying),
        LifecycleKind::Park => false,
        LifecycleKind::Unpark => state == Some(SessionState::Parked),
        LifecycleKind::Move | LifecycleKind::Restart | LifecycleKind::UpgradeContainer => {
            !matches!(
                state,
                Some(
                    SessionState::Running
                        | SessionState::Disconnected
                        | SessionState::Checkpointing
                        | SessionState::Closing
                )
            )
        }
        _ => true,
    }
}

/// Whether an operation admitted on a record in this durable state starts
/// committed, so that nothing can cancel it ([`LifecycleControl`]).
///
/// A failed startup's teardown must finish. A close or a Restart admitted on a
/// record that already says `Destroying` was past its point of no return
/// before this daemon started: the verified checkpoint is sealed and the
/// target is being removed, so stopping now only strands the target.
fn lifecycle_starts_committed(kind: LifecycleKind, state: Option<SessionState>) -> bool {
    kind == LifecycleKind::StartupCleanup
        || matches!(
            kind,
            LifecycleKind::Suspend | LifecycleKind::Restart | LifecycleKind::UpgradeContainer
        ) && state == Some(SessionState::Destroying)
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
/// `subagent` says the session is a Mjolnir sub-agent; see
/// [`crate::controller::has_nothing_to_checkpoint`].
fn close_route(session: Option<&SessionRecord>, subagent: bool) -> CloseRoute {
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
    } else if crate::controller::has_nothing_to_checkpoint(session, subagent) {
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
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum StartupStep {
    InstallHandoff(Box<mj_core::archive::CanonicalSessionSnapshot>),
    PreparedHandoff {
        text: String,
    },
    Prompt {
        text: String,
        inherited_draft: Option<String>,
    },
    Configure {
        key: String,
        value: String,
        optional: bool,
    },
    ApiPrompt {
        text: String,
    },
}

/// One supervised drain per session. Pending payloads stay in SQLite.
struct StartupQueue {
    identity: Arc<()>,
    last_error: Option<String>,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

struct ActiveLifecycle {
    /// Durable intent bound by Restart admission; cleanup names this exact owner.
    restart_intent: Option<String>,
    upgrade_work: Arc<Mutex<Option<crate::upgrade::Work>>>,
    phase: LifecyclePhase,
    operation_id: String,
    /// The one owner of whether this operation may still be cancelled.
    control: LifecycleControl,
    kind: LifecycleKind,
    started_at_epoch_seconds: u64,
    active_stages: BTreeMap<ProvisionStage, (usize, u64)>,
    /// The workspace a resume is claiming before its durable record changes.
    /// Workspace deletion consults this so it cannot race the claim.
    resume_workspace_id: Option<String>,
    resume_destination: Option<(String, String)>,
    notice: Option<String>,
    request_key: Option<String>,
    _move_guard: Option<MoveMutationGuard>,
    result: LifecycleWatch,
}

enum LifecyclePhase {
    Executing,
    RetiringContainer,
    CompletingRestart,
    CancellingRestart,
    MovingDestination,
    Cancelling,
    CancellingMoveDestination,
    Completed(LifecycleResult),
}

impl ActiveLifecycle {
    fn is_running(&self) -> bool {
        !matches!(self.phase, LifecyclePhase::Completed(_))
    }

    fn pending_upgrade_failed(&self) -> bool {
        self.kind == LifecycleKind::UpgradeContainer
            && self.restart_intent.is_some()
            && !self.control.is_committed()
            && matches!(self.phase, LifecyclePhase::Completed(Err(_)))
    }

    fn is_visible(&self) -> bool {
        self.is_running()
            || self.pending_upgrade_failed()
            || matches!(
                self.phase,
                LifecyclePhase::Completed(Ok(DaemonLifecycleResult::DeferredCleanup))
            )
    }

    fn request_cancel(&mut self) -> bool {
        self.cancel_with(LifecycleControl::request_cancel)
    }

    fn cancel_with(&mut self, cancel: fn(&LifecycleControl) -> bool) -> bool {
        if !self.pending_upgrade_failed()
            && !matches!(
                self.phase,
                LifecyclePhase::Executing | LifecyclePhase::MovingDestination
            )
        {
            return false;
        }
        let accepted = cancel(&self.control);
        if accepted {
            self.phase = match self.phase {
                LifecyclePhase::MovingDestination => LifecyclePhase::CancellingMoveDestination,
                _ => LifecyclePhase::Cancelling,
            };
        }
        accepted
    }

    fn is_cancellable(&self) -> bool {
        (self.pending_upgrade_failed()
            || matches!(
                self.phase,
                LifecyclePhase::Executing | LifecyclePhase::MovingDestination
            ))
            && self.control.is_cancellable()
    }

    /// Whether a person may cancel this operation now. The dashboard offers a
    /// cancel, and a person's cancel is accepted, only when this holds.
    ///
    /// Besides the operation's own control, a failed startup's cleanup keeps
    /// a person from cancelling whatever operation runs it: create, resume and
    /// unpark move their record to `StartupCleanup` in code that does not hold
    /// the control, and that cleanup ignores cancellation anyway.
    fn cancellable_by_person(&self, durable: Option<SessionState>) -> bool {
        self.person_may_cancel_kind(durable) && self.is_cancellable()
    }

    fn person_may_cancel_kind(&self, durable: Option<SessionState>) -> bool {
        !self.kind.cancelled_only_by_its_owner() && durable != Some(SessionState::StartupCleanup)
    }

    /// Stop this operation for daemon shutdown. An operation that can still be
    /// cancelled is cancelled. A committed one is interrupted, because the
    /// next daemon finishes it from its record, except a committed create,
    /// whose caller now owns the new session and which no daemon resumes.
    fn interrupt_for_shutdown(&mut self) -> bool {
        if self.kind == LifecycleKind::UpgradeContainer
            && matches!(self.phase, LifecyclePhase::RetiringContainer)
        {
            self.control.interrupt();
            return true;
        }
        if self.cancel_with(LifecycleControl::cancel_for_shutdown) {
            return true;
        }
        let interruptible = self.kind != LifecycleKind::Create
            && self.control.is_committed()
            && matches!(
                self.phase,
                LifecyclePhase::Executing | LifecyclePhase::MovingDestination
            );
        if interruptible {
            self.control.interrupt();
        }
        interruptible
    }
}

#[derive(Debug, Clone)]
enum DaemonLifecycleResult {
    Done,
    DeferredCleanup,
    Move(MoveOutcome),
    Park(crate::controller::ParkOutcome),
}

/// How one lifecycle operation ended when it failed.
///
/// The result is broadcast to every waiter, which is why it cannot simply be
/// the `anyhow::Error`: that is not clonable. Keeping the refusal beside the
/// text is what lets a reason written for the caller survive the crossing; a
/// failure rebuilt from a string alone would arrive as an internal fault.
#[derive(Debug, Clone)]
pub(crate) struct LifecycleFailure {
    pub(crate) detail: String,
    pub(crate) refusal: Option<Refusal>,
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
            LifecycleKind::Suspend => Self::Suspend,
            LifecycleKind::Resume => Self::Resume,
            LifecycleKind::Restart | LifecycleKind::UpgradeContainer => Self::Resume,
            LifecycleKind::Move => Self::Move,
            LifecycleKind::ForceStop => Self::ForceStop,
            LifecycleKind::DestroyStopped | LifecycleKind::ArchiveStopped => Self::DestroyStopped,
            LifecycleKind::ForceDestroy => Self::ForceDestroy,
            // A park is shown as the sub-agent stop it is, and an unpark as
            // the resume it is; neither needs a wire kind of its own.
            LifecycleKind::StopSubagent | LifecycleKind::Park => Self::StopSubagent,
            LifecycleKind::Unpark => Self::Resume,
            LifecycleKind::Cleanup | LifecycleKind::StartupCleanup => Self::Cleanup,
        }
    }
}

mod capacity;
mod close;
mod close_workspace;
mod create;
mod readiness;
use readiness::{HarnessReadinessWatch, ReadinessObservation, UnreadySession};
mod lifecycle;
mod resume;
mod snapshot;
pub(crate) mod startup_followup;
mod state;
mod subagent_park;
mod support;
mod views;
use support::*;
mod activity_cleanup;
pub(crate) mod delegation;
mod diagnostics;
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

mod continuation;
