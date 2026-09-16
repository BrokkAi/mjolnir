//! Multiplexed controller-side ownership of durable ACP relay sessions.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::{mpsc, oneshot, watch};

use crate::database::{
    ProjectionApplyOutcome, ProjectionIntegrityError, apply_projection_page,
    save_materialized_session,
};
use crate::worker_client::{RelayClient, RelayEventPage, RelayRejected, RelayTransportDead};
use mj_checkpoint::archive::verify_archive_streaming;
use mj_core::credentials::{CredentialSyncSignal, relay_event_credential_sync_reason};
use mj_core::elicitation::ElicitationResponse;
use mj_core::state::{ManagedSessionSnapshot, MaterializedSession};
use mj_transcript::projection::{
    ProjectionIndex, apply_committed_projection_event_indexed, materialized_session_from_canonical,
    project_relay_event_indexed,
};

use crate::targets::{
    CancellableProcessExecutor, CommandExecutor, CommandPlan, CommandSpec, TargetLocator,
    TargetRecoveryOutcome, TargetRecoveryPlan, ensure_recovery_target_running,
};
use mj_core::relay::{RelayCommand, RelayCursor, RelayOperationalState};

pub use mj_client::session::{
    ManagedSessionView, ReviewerAction, ReviewerOutcome, ViewError, new_command_id,
};
#[cfg(test)]
use mj_core::worker_launch::ReviewerLaunchConfig;

const SESSION_SYNC_INTERVAL: Duration = Duration::from_millis(150);
/// Release SQLite's single writer between bounded pieces of a large relay
/// catch-up. One transport page can contain thousands of terminal events and
/// must not prevent every other session actor from publishing its view.
const PROJECTION_TRANSACTION_EVENT_BUDGET: usize = 128;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
/// Ceiling for reconnect backoff. A worker that exited stays gone until the
/// user acts, so retrying it every second only burns process spawns.
const RECONNECT_BACKOFF_CEILING: Duration = Duration::from_secs(30);
const UNREACHABLE_FAILURE_THRESHOLD: u32 = 2;
const WORKER_RESTART_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_RESTART_COOLDOWN: Duration = Duration::from_secs(60);
const SESSION_MANAGER_SHUTDOWN_GRACE: Duration = Duration::from_millis(750);

#[derive(Debug)]
struct ProjectionAdvancedError {
    event_ordinal: u64,
}

impl std::fmt::Display for ProjectionAdvancedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "another projector committed relay event {} first",
            self.event_ordinal
        )
    }
}

impl std::error::Error for ProjectionAdvancedError {}

/// Delay before the next reconnect attempt after `failures` consecutive
/// failures. Doubles from `RECONNECT_INTERVAL` up to the ceiling.
fn reconnect_delay(failures: u32) -> Duration {
    let doubling = failures.saturating_sub(1).min(u32::BITS - 1);
    RECONNECT_INTERVAL
        .saturating_mul(1_u32 << doubling)
        .min(RECONNECT_BACKOFF_CEILING)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySessionTarget {
    pub session_id: String,
    pub spec: CommandSpec,
    /// Prove the exact worker is absent before restarting it in place. Direct
    /// relay clients omit recovery; controller-managed sessions self-heal
    /// without turning a shared transport outage into destructive restarts.
    pub worker_recovery: Option<WorkerRecoveryPlan>,
    pub project_memory: Option<ProjectMemorySyncTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMemorySyncTarget {
    pub canonical_root: std::path::PathBuf,
}

/// The working directory a bare-target worker must be able to enter before it
/// can serve a relay handshake. Container availability is checked separately
/// by the target recovery plan; bare targets have no runtime object to inspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerWorkspace {
    pub target: mj_core::state::ManagedWorktreeTarget,
    pub directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecoveryPlan {
    /// Durable target identity when this plan was built. A stale actor must
    /// never recover a resource after the session moves or starts destruction.
    pub source_target: mj_core::state::TargetLocator,
    pub target: Option<TargetRecoveryPlan>,
    pub workspace: Option<WorkerWorkspace>,
    pub liveness_probe: CommandSpec,
    /// Refresh a stale installed worker before restarting it. The digest is
    /// computed inside the recovery task so hashing a large binary never
    /// blocks a controller UI loop.
    pub binary_refresh: Option<WorkerBinaryRefresh>,
    /// Keep the worker executable and its launch schema paired. Configuration
    /// bytes travel through redacted stdin only when their digest is stale.
    pub launch_refresh: Option<WorkerLaunchRefreshPlan>,
    pub restart: CommandPlan,
}

/// How recovery refreshes a stale installed worker binary before restarting.
///
/// Local targets resolve the source and the copy at plan-build time, which is
/// cheap. Remote targets cannot: choosing the binary needs the target's
/// architecture, and that probe plus hashing the remote binary are blocking
/// ssh round-trips that must not run on the plan-build/UI path. So a remote
/// refresh carries only what is cheap to compute and resolves the rest inside
/// the recovery task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerBinaryRefresh {
    Prepared(WorkerBinaryRefreshPlan),
    Remote(RemoteWorkerBinaryRefresh),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBinaryRefreshPlan {
    pub source: PathBuf,
    pub installed_digest: CommandSpec,
    pub replace: CommandPlan,
}

/// A remote worker refresh resolved at recovery time: select the worker binary
/// for the target's own architecture, compare it to the installed one, and
/// copy only when they differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWorkerBinaryRefresh {
    pub locator: TargetLocator,
    pub session_id: String,
    pub installed_digest: CommandSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLaunchRefreshPlan {
    pub expected_sha256: String,
    pub installed_digest: CommandSpec,
    pub replace: CommandPlan,
}

/// Whether a relay failure means the transport to the worker is gone, so
/// restarting that worker is the only recovery left.
///
/// Every failure that proves it is marked with [`RelayTransportDead`] where it
/// is produced, and this decision downcasts for that marker. Message text is
/// never read: a reworded diagnostic must not be able to disable auto-restart.
pub(crate) fn worker_connect_needs_restart(error: &anyhow::Error) -> bool {
    RelayTransportDead::marks(error)
}

fn worker_connect_allows_live_restart(error: &anyhow::Error) -> bool {
    RelayTransportDead::marks_failed_handshake(error)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerRecoveryOutcome {
    Alive,
    Starting,
    TargetMissing,
    Suppressed,
    WorkspaceMissing(PathBuf),
    RestartedDead,
    RestartedUnresponsive,
}

fn refresh_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    refresh: Option<&WorkerBinaryRefresh>,
) -> Result<()> {
    match refresh {
        None => Ok(()),
        Some(WorkerBinaryRefresh::Prepared(plan)) => {
            let expected = mj_core::worker_launch::worker_executable_digest(&plan.source)?;
            if installed_digest_matches(executor, &plan.installed_digest, &expected) {
                return Ok(());
            }
            plan.replace
                .execute(executor)
                .context("replace stale relay worker binary")?;
            Ok(())
        }
        // Remote: pick the binary for the target's architecture and copy only
        // if it differs. Runs here in the recovery task, never on the UI path.
        Some(WorkerBinaryRefresh::Remote(refresh)) => {
            crate::controller::refresh_remote_worker_binary_if_stale(executor, refresh)
        }
    }
}

fn installed_digest_matches(
    executor: &impl CommandExecutor,
    command: &CommandSpec,
    expected: &str,
) -> bool {
    executor.execute(command).as_ref().is_ok_and(|output| {
        output.status == 0
            && String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .is_some_and(|digest| digest.eq_ignore_ascii_case(expected))
    })
}

fn refresh_worker_launch_if_stale(
    executor: &impl CommandExecutor,
    plan: Option<&WorkerLaunchRefreshPlan>,
) -> Result<()> {
    let Some(plan) = plan else {
        return Ok(());
    };
    if installed_digest_matches(executor, &plan.installed_digest, &plan.expected_sha256) {
        return Ok(());
    }
    plan.replace
        .execute(executor)
        .context("replace stale relay worker launch config")?;
    Ok(())
}

#[cfg(test)]
async fn recover_worker(
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
) -> Result<WorkerRecoveryOutcome> {
    recover_worker_for_session(plan, restart_unresponsive, None).await
}

async fn recover_worker_for_session(
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
    session_id: Option<String>,
) -> Result<WorkerRecoveryOutcome> {
    tokio::task::spawn_blocking(move || {
        let executor = CancellableProcessExecutor::with_timeout(WORKER_RESTART_TIMEOUT);
        recover_worker_controlled(plan, restart_unresponsive, session_id.as_deref(), &executor)
    })
    .await
    .context("worker recovery task failed")?
}

pub(crate) fn recover_worker_controlled(
    mut plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
    session_id: Option<&str>,
    executor: &impl CommandExecutor,
) -> Result<WorkerRecoveryOutcome> {
    let target_mutex = session_id.map(crate::recovery_gate::worker_target_mutex);
    let _target_guard = target_mutex
        .as_ref()
        .map(|lock| {
            lock.lock()
                .map_err(|_| anyhow::anyhow!("worker target ownership lock poisoned"))
        })
        .transpose()?;
    if let Some(id) = session_id {
        let state =
            crate::database::load_state().context("read durable session before worker recovery")?;
        let eligible = state.sessions.get(id).is_some_and(|session| {
            crate::pollers::session_target_is_pollable(session)
                && session.target.as_ref() == Some(&plan.source_target)
        });
        if !eligible || crate::controller::move_session::move_owns_session(id) {
            return Ok(WorkerRecoveryOutcome::Suppressed);
        }
    }
    // A failed Move can leave this actor with a plan from before recovery.
    // Never overwrite the durable checkpoint-only launch with that old plan.
    if let Some(id) = session_id
        && crate::database::load_move_operation(id)?
            .is_some_and(|op| op.source_checkpoint_only && op.destination_target.is_none())
    {
        plan = crate::controller::Controller::load()?.worker_recovery_plan(id)?;
    }
    if ensure_recovery_target_running(executor, plan.target.as_ref())
        .context("restore relay worker target")?
        == TargetRecoveryOutcome::Missing
    {
        return Ok(WorkerRecoveryOutcome::TargetMissing);
    }
    let output = executor
        .execute(&plan.liveness_probe)
        .context("probe relay worker liveness")?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            plan.liveness_probe.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "starting" => Ok(WorkerRecoveryOutcome::Starting),
        "alive" if !restart_unresponsive => Ok(WorkerRecoveryOutcome::Alive),
        "alive" => {
            if let Some(workspace) = plan.workspace.as_ref()
                && !crate::controller::path_exists_on_managed_target(
                    executor,
                    &workspace.target,
                    &workspace.directory,
                )?
            {
                return Ok(WorkerRecoveryOutcome::WorkspaceMissing(
                    workspace.directory.clone(),
                ));
            }
            refresh_worker_binary_if_stale(executor, plan.binary_refresh.as_ref())?;
            refresh_worker_launch_if_stale(executor, plan.launch_refresh.as_ref())?;
            plan.restart.execute(executor)?;
            Ok(WorkerRecoveryOutcome::RestartedUnresponsive)
        }
        "dead" => {
            if let Some(workspace) = plan.workspace.as_ref()
                && !crate::controller::path_exists_on_managed_target(
                    executor,
                    &workspace.target,
                    &workspace.directory,
                )?
            {
                return Ok(WorkerRecoveryOutcome::WorkspaceMissing(
                    workspace.directory.clone(),
                ));
            }
            refresh_worker_binary_if_stale(executor, plan.binary_refresh.as_ref())?;
            refresh_worker_launch_if_stale(executor, plan.launch_refresh.as_ref())?;
            plan.restart.execute(executor)?;
            Ok(WorkerRecoveryOutcome::RestartedDead)
        }
        output => bail!("worker liveness probe returned unexpected output {output:?}"),
    }
}

#[derive(Debug, Clone)]
pub struct SessionManagerUpdate {
    pub session_id: String,
    pub view: ManagedSessionView,
}

pub struct SessionManagerChannels {
    pub targets: watch::Sender<Vec<RelaySessionTarget>>,
    pub control: SessionManagerControl,
    pub updates: SessionManagerUpdates,
    pub shutdown: SessionManagerShutdown,
}

/// Client-side half of a remotely owned session manager.
///
/// The daemon remains the only process with relay connections. A control
/// surface publishes the daemon's latest views here and forwards requests from
/// [`RemoteSessionRequests`] over its authenticated transport.
pub struct RemoteSessionManagerChannels {
    pub targets: watch::Sender<Vec<RelaySessionTarget>>,
    pub control: SessionManagerControl,
    pub updates: SessionManagerUpdates,
    pub shutdown: SessionManagerShutdown,
    pub publisher: RemoteSessionPublisher,
    pub requests: RemoteSessionRequests,
}

#[derive(Clone)]
pub struct RemoteSessionPublisher {
    updates: mpsc::UnboundedSender<RemoteManagerUpdate>,
}

impl RemoteSessionPublisher {
    pub async fn publish(&self, session_id: String, view: ManagedSessionView) -> Result<()> {
        self.updates
            .send(RemoteManagerUpdate::Publish { session_id, view })
            .context("remote session manager stopped")
    }

    pub fn try_publish(&self, session_id: String, view: ManagedSessionView) -> Result<()> {
        self.updates
            .send(RemoteManagerUpdate::Publish { session_id, view })
            .context("remote session manager update queue is unavailable")
    }
}

pub struct RemoteSessionRequests {
    requests: mpsc::Receiver<RemoteSessionRequest>,
}

impl RemoteSessionRequests {
    pub async fn recv(&mut self) -> Option<RemoteSessionRequest> {
        self.requests.recv().await
    }
}

pub enum RemoteSessionRequest {
    Submit {
        session_id: String,
        command_id: String,
        command: RelayCommand,
        admission: Option<ReviewDeliveryAdmission>,
        reply: oneshot::Sender<std::result::Result<u64, String>>,
    },
    Sync {
        session_id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    RespondElicitation {
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    StopBackgroundTask {
        session_id: String,
        background_task_id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    Reviewer {
        session_id: String,
        /// Which reviewing role the action drives; `None` is the default one.
        role: Option<String>,
        action: ReviewerAction,
        reply: oneshot::Sender<std::result::Result<ReviewerOutcome, String>>,
    },
}

impl RemoteSessionRequest {
    /// The session this request acts on. Requests for one session have to be
    /// carried out in the order they were made.
    pub fn session_id(&self) -> &str {
        match self {
            Self::Submit { session_id, .. }
            | Self::Sync { session_id, .. }
            | Self::RespondElicitation { session_id, .. }
            | Self::StopBackgroundTask { session_id, .. }
            | Self::Reviewer { session_id, .. } => session_id,
        }
    }
}

/// Keeps each session's relay requests in the order they were made, while
/// letting different sessions overlap.
///
/// A bridge that spawns every request concurrently loses the order the caller
/// submitted them in, and the order is load-bearing: `/effort` followed by a
/// prompt has to reach the relay that way round, or the prompt runs under the
/// old setting. Awaiting each request inline would restore the order but would
/// also make one slow session block every other one, so instead each request
/// waits on its own session's previous request and nothing else.
#[derive(Default)]
pub struct SessionRequestOrder {
    latest: std::collections::HashMap<SessionRequestStream, tokio::task::JoinHandle<()>>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum SessionRequestStream {
    Primary(String),
    Reviewer(String, Option<String>),
}

impl SessionRequestOrder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `forward` for `request` after everything already queued for the
    /// same primary or reviewer role has finished. Independent reviewers
    /// must not delay primary controls or one another.
    pub fn dispatch<F, Fut>(&mut self, request: RemoteSessionRequest, forward: F)
    where
        F: FnOnce(RemoteSessionRequest) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        // Sessions that have gone quiet leave a finished handle behind; drop
        // them here so the map tracks live work rather than every session the
        // bridge has ever served.
        self.latest.retain(|_, handle| !handle.is_finished());
        let stream = match &request {
            RemoteSessionRequest::Reviewer {
                session_id, role, ..
            } => SessionRequestStream::Reviewer(session_id.clone(), role.clone()),
            _ => SessionRequestStream::Primary(request.session_id().to_owned()),
        };
        let previous = self.latest.remove(&stream);
        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                // A panicked predecessor still releases its successor: the
                // request behind it is the user's, and dropping it silently
                // would be worse than running it late.
                if let Err(error) = previous.await {
                    tracing::error!(%error, "previous session request task failed");
                }
            }
            forward(request).await;
        });
        self.latest.insert(stream, handle);
    }
}

/// Exclusive owner of the manager task and every relay actor below it.
///
/// Long-running control surfaces explicitly await [`Self::shutdown`] before
/// their Tokio runtime goes away. Drop remains an aborting fallback for tests
/// and early-return paths that cannot await.
pub struct SessionManagerShutdown {
    signal: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SessionManagerShutdown {
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(signal) = self.signal.take() {
            let _ = signal.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.context("session manager shutdown task failed")?;
        }
        Ok(())
    }
}

impl Drop for SessionManagerShutdown {
    fn drop(&mut self) {
        if let Some(signal) = self.signal.take() {
            let _ = signal.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
struct CoalescedUpdateSender {
    pending: Arc<Mutex<BTreeMap<String, SessionManagerUpdate>>>,
    wake: mpsc::Sender<()>,
}

/// Bounded latest-state feed for the dashboard. At most one snapshot per
/// session is retained while the consumer is busy.
pub struct SessionManagerUpdates {
    pending: Arc<Mutex<BTreeMap<String, SessionManagerUpdate>>>,
    wake: mpsc::Receiver<()>,
}

impl CoalescedUpdateSender {
    fn send(&self, update: SessionManagerUpdate) {
        if self.wake.is_closed() {
            return;
        }
        self.pending
            .lock()
            .expect("session update coalescer poisoned")
            .insert(update.session_id.clone(), update);
        let _ = self.wake.try_send(());
    }
}

impl SessionManagerUpdates {
    fn pop_pending(&self) -> Option<SessionManagerUpdate> {
        self.pending
            .lock()
            .expect("session update coalescer poisoned")
            .pop_first()
            .map(|(_, update)| update)
    }

    pub async fn recv(&mut self) -> Option<SessionManagerUpdate> {
        loop {
            if let Some(update) = self.pop_pending() {
                return Some(update);
            }
            self.wake.recv().await?;
        }
    }

    pub fn try_recv(
        &mut self,
    ) -> std::result::Result<SessionManagerUpdate, mpsc::error::TryRecvError> {
        if let Some(update) = self.pop_pending() {
            return Ok(update);
        }
        self.wake.try_recv()?;
        self.pop_pending().ok_or(mpsc::error::TryRecvError::Empty)
    }
}

fn coalesced_update_channel() -> (CoalescedUpdateSender, SessionManagerUpdates) {
    let pending = Arc::new(Mutex::new(BTreeMap::new()));
    let (wake_tx, wake_rx) = mpsc::channel(1);
    (
        CoalescedUpdateSender {
            pending: pending.clone(),
            wake: wake_tx,
        },
        SessionManagerUpdates {
            pending,
            wake: wake_rx,
        },
    )
}

#[derive(Clone)]
pub struct SessionManagerControl {
    commands: mpsc::Sender<ManagerCommand>,
}

#[derive(Clone, Debug)]
pub struct ManagedSessionHandle {
    session_id: String,
    commands: mpsc::Sender<ActorCommand>,
    releases: mpsc::UnboundedSender<ReturnedConnection>,
    view: watch::Receiver<ManagedSessionView>,
}

/// A one-command capability issued by the review host while its prompt hold
/// is open. It is intentionally opaque to callers: the session actor checks
/// it against the host's live hold registry before bypassing prompt refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDeliveryAdmission {
    session_id: String,
    epoch: u64,
    command_id: String,
}

impl ReviewDeliveryAdmission {
    pub(crate) fn new(session_id: String, epoch: u64, command_id: String) -> Self {
        Self {
            session_id,
            epoch,
            command_id,
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn command_id(&self) -> &str {
        &self.command_id
    }
}

/// Exclusive ownership of a session actor's existing relay connection.
///
/// Lifecycle operations use this instead of opening a competing projection
/// client. Dropping an unreleased lease drops the proxy connection, which in
/// turn cancels any ordinary relay checkpoint barrier.
///
/// Prompt submissions that arrive while the lease is active are not rejected.
/// The actor queues them and forwards them in arrival order once the lease is
/// released or dropped.
pub struct ManagedSessionLease {
    session_id: String,
    lease_id: Option<u64>,
    connection: Option<StandaloneSession>,
    releases: mpsc::UnboundedSender<ReturnedConnection>,
}

impl ManagedSessionLease {
    pub fn connection_mut(&mut self) -> &mut StandaloneSession {
        self.connection
            .as_mut()
            .expect("managed session lease has already been released")
    }

    /// Swap the leased proxy after the worker process behind it was replaced.
    /// The actor stays leased, so queued prompts cannot race the new latch.
    pub fn replace_connection(&mut self, connection: StandaloneSession) {
        drop(self.connection.take());
        self.connection = Some(connection);
    }

    pub fn release(mut self) {
        let lease_id = self
            .lease_id
            .take()
            .expect("managed session lease has already been released");
        let connection = self.connection.take();
        if let Err(error) = self.releases.send(ReturnedConnection {
            lease_id,
            connection,
        }) {
            tracing::warn!(
                session_id = %self.session_id,
                operation = "lease_release",
                %error,
                "session actor stopped before receiving released relay connection"
            );
        }
    }
}

impl Drop for ManagedSessionLease {
    fn drop(&mut self) {
        let Some(lease_id) = self.lease_id.take() else {
            return;
        };
        // Drop the proxy before telling the actor to reconnect so the relay
        // observes EOF and releases any abandoned checkpoint barrier first.
        drop(self.connection.take());
        if let Err(error) = self.releases.send(ReturnedConnection {
            lease_id,
            connection: None,
        }) {
            tracing::warn!(
                session_id = %self.session_id,
                operation = "lease_drop",
                %error,
                "session actor stopped before receiving dropped relay lease"
            );
        }
    }
}

impl ManagedSessionHandle {
    /// Narrow this controller-owned handle to the operations a control surface uses.
    pub fn client(&self) -> mj_client::session::SessionHandle {
        mj_client::session::SessionHandle::new(ClientSessionHandle(self.clone()))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn view(&self) -> ManagedSessionView {
        self.view.borrow().clone()
    }

    /// Whether the per-session actor behind this handle has retired. The
    /// manager itself may still be alive with a replacement actor, so callers
    /// holding long-lived handles use this to reacquire the current one.
    pub fn is_stopped(&self) -> bool {
        self.commands.is_closed()
    }

    pub fn has_changed(&self) -> Result<bool> {
        self.view.has_changed().context("session manager stopped")
    }

    pub async fn changed(&mut self) -> Result<ManagedSessionView> {
        self.view
            .changed()
            .await
            .context("session manager stopped")?;
        Ok(self.view())
    }

    pub async fn submit(&self, command_id: String, command: RelayCommand) -> Result<u64> {
        self.enqueue_submit(command_id, command).await?.wait().await
    }

    /// Submit the review's corrective prompt through the one admission that
    /// corresponds to its live prompt hold. Generic submissions continue to
    /// use [`Self::submit`] and remain subject to review refusal.
    pub(crate) async fn submit_review_delivery(
        &self,
        admission: ReviewDeliveryAdmission,
        command: RelayCommand,
    ) -> Result<u64> {
        let command_id = admission.command_id.clone();
        self.enqueue_submit_with_admission(command_id, command, Some(admission))
            .await?
            .wait()
            .await
    }

    pub async fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> Result<PendingRelaySubmit> {
        self.enqueue_submit_with_admission(command_id, command, None)
            .await
    }

    async fn enqueue_submit_with_admission(
        &self,
        command_id: String,
        command: RelayCommand,
        admission: Option<ReviewDeliveryAdmission>,
    ) -> Result<PendingRelaySubmit> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Submit {
                command_id,
                command,
                admission,
                reply,
            })
            .await
            .context("session manager stopped")?;
        Ok(PendingRelaySubmit { response })
    }

    pub async fn sync_now(&self) -> Result<()> {
        self.enqueue_sync().await?.wait().await
    }

    pub async fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(ActorCommand::RespondElicitation {
                elicitation_id,
                response,
                reply,
            })
            .await
            .context("session manager stopped")?;
        result
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }

    pub async fn stop_background_task(&self, background_task_id: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(ActorCommand::StopBackgroundTask {
                background_task_id,
                reply,
            })
            .await
            .context("session manager stopped")?;
        result
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }

    /// Drive the session's second-opinion reviewer.
    ///
    /// The reviewer shares this session's relay connection, so its actions
    /// queue behind the session's own and are refused while a lifecycle
    /// operation holds the connection.
    pub async fn reviewer(&self, action: ReviewerAction) -> Result<ReviewerOutcome> {
        self.reviewer_as(None, action).await
    }

    /// Drive one reviewing role. `None` is the default role, which is the one
    /// plan review uses; a turn review in the extended tier names its
    /// supervisor, its intent analyst, and each specialist lane.
    pub async fn reviewer_as(
        &self,
        role: Option<String>,
        action: ReviewerAction,
    ) -> Result<ReviewerOutcome> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(ActorCommand::Reviewer {
                role,
                action,
                reply,
            })
            .await
            .context("session manager stopped")?;
        result
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }

    pub async fn enqueue_sync(&self) -> Result<PendingRelaySync> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Sync { reply })
            .await
            .context("session manager stopped")?;
        Ok(PendingRelaySync { response })
    }

    pub async fn lease_connection(&self) -> Result<ManagedSessionLease> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Lease { reply })
            .await
            .context("session manager stopped")?;
        let (lease_id, connection) = response.await.context("session manager stopped")??;
        Ok(ManagedSessionLease {
            session_id: self.session_id.clone(),
            lease_id: Some(lease_id),
            connection: Some(connection),
            releases: self.releases.clone(),
        })
    }
}

pub struct PendingRelaySubmit {
    response: oneshot::Receiver<std::result::Result<u64, String>>,
}

impl PendingRelaySubmit {
    pub async fn wait(self) -> Result<u64> {
        self.response
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }
}

pub struct PendingRelaySync {
    response: oneshot::Receiver<std::result::Result<(), String>>,
}

impl PendingRelaySync {
    pub async fn wait(self) -> Result<()> {
        self.response
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }
}

#[derive(Clone)]
struct ClientSessionHandle(ManagedSessionHandle);

impl mj_client::session::SessionHandleBackend for ClientSessionHandle {
    fn search_prompts(
        &self,
        bundle_id: String,
        scope: mj_core::storage::HistoryScope,
        query: String,
    ) -> mj_client::session::BoxFuture<'_, Result<Vec<mj_core::storage::PromptHistoryEntry>>> {
        let session_id = self.0.session_id().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::database::search_prompts(&session_id, &bundle_id, scope, &query)
            })
            .await
            .context("history search task")?
        })
    }
    fn review_state(
        &self,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::ReviewState>> {
        let session_id = self.0.session_id().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                Ok(mj_client::session::ReviewState {
                    review: crate::database::active_review(&session_id)?,
                    defaults: crate::database::reviewer_defaults()?,
                })
            })
            .await
            .context("review restoration task")?
        })
    }

    fn config_result(
        &self,
        command_id: String,
    ) -> mj_client::session::BoxFuture<'_, Result<Option<Option<String>>>> {
        let session_id = self.session_id().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::database::load_config_result(&session_id, &command_id)
            })
            .await
            .context("read configuration completion task")?
        })
    }

    fn clone_box(&self) -> Box<dyn mj_client::session::SessionHandleBackend> {
        Box::new(self.clone())
    }

    fn session_id(&self) -> &str {
        self.0.session_id()
    }

    fn view(&self) -> ManagedSessionView {
        self.0.view()
    }

    fn is_stopped(&self) -> bool {
        self.0.is_stopped()
    }

    fn has_changed(&self) -> Result<bool> {
        self.0.has_changed()
    }

    fn changed(&mut self) -> mj_client::session::BoxFuture<'_, Result<ManagedSessionView>> {
        Box::pin(self.0.changed())
    }

    fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::PendingRelaySubmit>> {
        Box::pin(async move {
            let pending = self.0.enqueue_submit(command_id, command).await?;
            Ok(mj_client::session::PendingRelaySubmit::new(Box::pin(
                pending.wait(),
            )))
        })
    }

    fn enqueue_sync(
        &self,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::PendingRelaySync>> {
        Box::pin(async move {
            let pending = self.0.enqueue_sync().await?;
            Ok(mj_client::session::PendingRelaySync::new(Box::pin(
                pending.wait(),
            )))
        })
    }

    fn respond_elicitation(
        &self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> mj_client::session::BoxFuture<'_, Result<()>> {
        Box::pin(self.0.respond_elicitation(elicitation_id, response))
    }

    fn stop_background_task(
        &self,
        background_task_id: String,
    ) -> mj_client::session::BoxFuture<'_, Result<()>> {
        Box::pin(self.0.stop_background_task(background_task_id))
    }

    fn reviewer(
        &self,
        role: Option<String>,
        action: ReviewerAction,
    ) -> mj_client::session::BoxFuture<'_, Result<ReviewerOutcome>> {
        Box::pin(self.0.reviewer_as(role, action))
    }
}

#[derive(Clone)]
struct ClientSessionControl(SessionManagerControl);

impl mj_client::session::SessionControlBackend for ClientSessionControl {
    fn session(
        &self,
        session_id: String,
    ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::SessionHandle>> {
        Box::pin(async move { Ok(self.0.session(session_id).await?.client()) })
    }
}

impl SessionManagerControl {
    /// Narrow this controller-owned manager to session lookup for a control surface.
    pub fn client(&self) -> mj_client::session::SessionControl {
        mj_client::session::SessionControl::new(ClientSessionControl(self.clone()))
    }

    pub async fn session(&self, session_id: impl Into<String>) -> Result<ManagedSessionHandle> {
        let session_id = session_id.into();
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ManagerCommand::Session {
                session_id: session_id.clone(),
                reply,
            })
            .await
            .context("session manager stopped")?;
        response
            .await
            .context("session manager stopped")?
            .with_context(|| format!("session {session_id} is not managed"))
    }

    pub async fn wait_for_session(
        &self,
        session_id: &str,
        timeout: Duration,
    ) -> Result<ManagedSessionHandle> {
        tokio::time::timeout(timeout, async {
            loop {
                match self.session(session_id.to_owned()).await {
                    Ok(handle) => return Ok(handle),
                    Err(error) => {
                        tracing::trace!(session_id, "waiting for session actor: {error:#}");
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "session {session_id} did not become available within {} seconds",
                timeout.as_secs()
            )
        })?
    }
}

enum ManagerCommand {
    Session {
        session_id: String,
        reply: oneshot::Sender<Option<ManagedSessionHandle>>,
    },
}

enum ActorCommand {
    Submit {
        command_id: String,
        command: RelayCommand,
        admission: Option<ReviewDeliveryAdmission>,
        reply: oneshot::Sender<std::result::Result<u64, String>>,
    },
    Sync {
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    RespondElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    StopBackgroundTask {
        background_task_id: String,
        reply: oneshot::Sender<std::result::Result<(), String>>,
    },
    Reviewer {
        role: Option<String>,
        action: ReviewerAction,
        reply: oneshot::Sender<std::result::Result<ReviewerOutcome, String>>,
    },
    /// The connection is handed over whole, and so is the failure: a caller
    /// that must decide whether to restart the worker needs the typed cause,
    /// which formatting the error to a string would destroy.
    Lease {
        reply: oneshot::Sender<Result<(u64, StandaloneSession)>>,
    },
}

impl ActorCommand {
    fn operation_name(&self) -> &'static str {
        match self {
            Self::Submit { .. } => "submit",
            Self::Sync { .. } => "sync",
            Self::RespondElicitation { .. } => "respond_elicitation",
            Self::StopBackgroundTask { .. } => "stop_background_task",
            Self::Reviewer { action, .. } => action.operation_name(),
            Self::Lease { .. } => "lease",
        }
    }

    fn reject(self, session_id: &str, message: &str) {
        match self {
            Self::Submit { reply, .. } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "submit",
                        "submit rejection receiver was already closed"
                    );
                }
            }
            Self::Sync { reply } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "sync",
                        "sync rejection receiver was already closed"
                    );
                }
            }
            Self::RespondElicitation { reply, .. } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "respond_elicitation",
                        "elicitation rejection receiver was already closed"
                    );
                }
            }
            Self::StopBackgroundTask { reply, .. } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "stop_background_task",
                        "background task stop rejection receiver was already closed"
                    );
                }
            }
            Self::Reviewer { reply, .. } => {
                if reply.send(Err(message.to_owned())).is_err() {
                    tracing::debug!(
                        %session_id,
                        operation = "reviewer",
                        "reviewer rejection receiver was already closed"
                    );
                }
            }
            Self::Lease { reply } => {
                if reply
                    .send(Err(anyhow::anyhow!(message.to_owned())))
                    .is_err()
                {
                    tracing::debug!(
                        %session_id,
                        operation = "lease",
                        "lease rejection receiver was already closed"
                    );
                }
            }
        }
    }
}

struct ReturnedConnection {
    lease_id: u64,
    connection: Option<StandaloneSession>,
}

/// A submission that arrived while a lifecycle operation held the connection.
/// The actor replays these in arrival order once the lease comes back.
struct DeferredSubmit {
    command_id: String,
    command: RelayCommand,
    admission: Option<ReviewDeliveryAdmission>,
    reply: oneshot::Sender<std::result::Result<u64, String>>,
}

#[derive(Debug, Default)]
struct ActorLifecycle {
    active_lease: Option<u64>,
    retirement_requested: bool,
}

impl ActorLifecycle {
    fn set_retirement_requested(&mut self, requested: bool) {
        self.retirement_requested = requested;
    }

    fn is_leased(&self) -> bool {
        self.active_lease.is_some()
    }

    fn should_stop(&self) -> bool {
        self.retirement_requested && !self.is_leased()
    }

    fn accepts_new_work(&self) -> bool {
        !self.retirement_requested
    }

    fn activate_lease(&mut self, lease_id: u64) {
        debug_assert!(self.active_lease.is_none());
        self.active_lease = Some(lease_id);
    }

    fn return_lease(&mut self, lease_id: u64) -> bool {
        if self.active_lease != Some(lease_id) {
            return false;
        }
        self.active_lease = None;
        true
    }
}

struct ActorRegistration {
    target: RelaySessionTarget,
    commands: mpsc::Sender<ActorCommand>,
    releases: mpsc::UnboundedSender<ReturnedConnection>,
    retirement: watch::Sender<bool>,
    view: watch::Receiver<ManagedSessionView>,
    abort: tokio::task::AbortHandle,
}

struct RemoteActorRegistration {
    commands: mpsc::Sender<ActorCommand>,
    releases: mpsc::UnboundedSender<ReturnedConnection>,
    view: watch::Receiver<ManagedSessionView>,
    view_tx: watch::Sender<ManagedSessionView>,
    abort: tokio::task::AbortHandle,
}

enum RemoteManagerUpdate {
    Publish {
        session_id: String,
        view: ManagedSessionView,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileAction {
    Idle,
    Spawn,
    Keep,
    Retire,
}

fn reconcile_action(
    actor: Option<&RelaySessionTarget>,
    desired: Option<&RelaySessionTarget>,
) -> ReconcileAction {
    match (actor, desired) {
        (None, None) => ReconcileAction::Idle,
        (None, Some(_)) => ReconcileAction::Spawn,
        (Some(actor), Some(desired)) if actor == desired => ReconcileAction::Keep,
        (Some(_), Some(_) | None) => ReconcileAction::Retire,
    }
}

fn target_map(targets: &[RelaySessionTarget]) -> BTreeMap<String, RelaySessionTarget> {
    targets
        .iter()
        .cloned()
        .map(|target| (target.session_id.clone(), target))
        .collect()
}

fn remove_actor_task(
    actors: &mut BTreeMap<String, ActorRegistration>,
    task_id: tokio::task::Id,
) -> Option<String> {
    let session_id = actors.iter().find_map(|(session_id, actor)| {
        (actor.abort.id() == task_id).then(|| session_id.clone())
    })?;
    actors.remove(&session_id);
    Some(session_id)
}

fn reconcile_actors(
    targets: &BTreeMap<String, RelaySessionTarget>,
    actors: &mut BTreeMap<String, ActorRegistration>,
    tasks: &mut tokio::task::JoinSet<String>,
    updates: &CoalescedUpdateSender,
) {
    // A completed or cancelled task closes its command receiver before the
    // JoinSet completion necessarily wins the manager's select. Do not let
    // that dead registration suppress the replacement this reconciliation is
    // responsible for starting. Task-ID-aware completion cleanup below keeps
    // the old completion from removing the replacement later.
    actors.retain(|session_id, actor| {
        let live = !actor.commands.is_closed();
        if !live {
            tracing::warn!(session_id, "replacing stopped session relay actor");
        }
        live
    });

    for (session_id, actor) in actors.iter() {
        let retiring = matches!(
            reconcile_action(Some(&actor.target), targets.get(session_id)),
            ReconcileAction::Retire
        );
        actor.retirement.send_replace(retiring);
    }

    for (session_id, target) in targets {
        if !matches!(
            reconcile_action(
                actors.get(session_id).map(|actor| &actor.target),
                Some(target)
            ),
            ReconcileAction::Spawn
        ) {
            continue;
        }
        let (actor_tx, actor_rx) = mpsc::channel(32);
        let (release_tx, release_rx) = mpsc::unbounded_channel();
        let (retirement_tx, retirement_rx) = watch::channel(false);
        let (view_tx, view_rx) = watch::channel(ManagedSessionView::default());
        let actor_updates = updates.clone();
        let task_target = target.clone();
        let task_id = session_id.clone();
        let abort = tasks.spawn(async move {
            run_session_actor(
                task_target,
                actor_rx,
                release_rx,
                retirement_rx,
                view_tx,
                actor_updates,
            )
            .await;
            task_id
        });
        actors.insert(
            session_id.clone(),
            ActorRegistration {
                target: target.clone(),
                commands: actor_tx,
                releases: release_tx,
                retirement: retirement_tx,
                view: view_rx,
                abort,
            },
        );
    }
}

async fn run_remote_session_actor(
    session_id: String,
    mut commands: mpsc::Receiver<ActorCommand>,
    requests: mpsc::Sender<RemoteSessionRequest>,
) {
    while let Some(command) = commands.recv().await {
        let request = match command {
            ActorCommand::Submit {
                command_id,
                command,
                admission,
                reply,
            } => RemoteSessionRequest::Submit {
                session_id: session_id.clone(),
                command_id,
                command,
                admission,
                reply,
            },
            ActorCommand::Sync { reply } => RemoteSessionRequest::Sync {
                session_id: session_id.clone(),
                reply,
            },
            ActorCommand::RespondElicitation {
                elicitation_id,
                response,
                reply,
            } => RemoteSessionRequest::RespondElicitation {
                session_id: session_id.clone(),
                elicitation_id,
                response,
                reply,
            },
            ActorCommand::StopBackgroundTask {
                background_task_id,
                reply,
            } => RemoteSessionRequest::StopBackgroundTask {
                session_id: session_id.clone(),
                background_task_id,
                reply,
            },
            ActorCommand::Reviewer {
                role,
                action,
                reply,
            } => RemoteSessionRequest::Reviewer {
                session_id: session_id.clone(),
                role,
                action,
                reply,
            },
            ActorCommand::Lease { reply } => {
                let _ = reply.send(Err(anyhow::anyhow!(
                    "relay connection leases are available only inside the controller daemon"
                )));
                continue;
            }
        };
        if let Err(error) = requests.send(request).await {
            match error.0 {
                RemoteSessionRequest::Submit { reply, .. } => {
                    let _ = reply.send(Err("controller daemon request bridge stopped".into()));
                }
                RemoteSessionRequest::Sync { reply, .. }
                | RemoteSessionRequest::RespondElicitation { reply, .. }
                | RemoteSessionRequest::StopBackgroundTask { reply, .. } => {
                    let _ = reply.send(Err("controller daemon request bridge stopped".into()));
                }
                RemoteSessionRequest::Reviewer { reply, .. } => {
                    let _ = reply.send(Err("controller daemon request bridge stopped".into()));
                }
            }
            break;
        }
    }
}

fn spawn_remote_actor(
    session_id: String,
    view: ManagedSessionView,
    requests: &mpsc::Sender<RemoteSessionRequest>,
    actors: &mut BTreeMap<String, RemoteActorRegistration>,
    updates: &CoalescedUpdateSender,
) {
    let (actor_tx, actor_rx) = mpsc::channel(32);
    let (release_tx, _release_rx) = mpsc::unbounded_channel();
    let (view_tx, view_rx) = watch::channel(view.clone());
    let abort = tokio::spawn(run_remote_session_actor(
        session_id.clone(),
        actor_rx,
        requests.clone(),
    ))
    .abort_handle();
    actors.insert(
        session_id.clone(),
        RemoteActorRegistration {
            commands: actor_tx,
            releases: release_tx,
            view: view_rx,
            view_tx,
            abort,
        },
    );
    updates.send(SessionManagerUpdate { session_id, view });
}

/// Build the read/control facade used by a control surface whose relay actors
/// live in another process. Target updates still decide which session handles
/// exist, while [`RemoteSessionPublisher`] supplies their latest views.
pub fn spawn_remote_session_manager() -> Result<RemoteSessionManagerChannels> {
    let (targets_tx, mut targets_rx) = watch::channel(Vec::<RelaySessionTarget>::new());
    let (commands_tx, mut commands_rx) = mpsc::channel(32);
    let (updates_tx, updates_rx) = coalesced_update_channel();
    let (published_tx, mut published_rx) = mpsc::unbounded_channel();
    let (requests_tx, requests_rx) = mpsc::channel(64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut actors = BTreeMap::<String, RemoteActorRegistration>::new();
        let mut latest = BTreeMap::<String, ManagedSessionView>::new();
        let mut desired = BTreeMap::<String, RelaySessionTarget>::new();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    desired = target_map(&targets_rx.borrow_and_update());
                    actors.retain(|session_id, actor| {
                        if desired.contains_key(session_id) {
                            true
                        } else {
                            actor.abort.abort();
                            false
                        }
                    });
                    // Drop the reseed view for every session that is no longer a
                    // live target. `latest` is only ever inserted into otherwise,
                    // so without this it keeps a full MaterializedSession per
                    // session ever seen — a slow memory leak the actor
                    // reconciliation above does not cover.
                    latest.retain(|session_id, _| desired.contains_key(session_id));
                    for session_id in desired.keys() {
                        if !actors.contains_key(session_id)
                            && let Some(view) = latest.get(session_id).cloned()
                        {
                            spawn_remote_actor(
                                session_id.clone(),
                                view,
                                &requests_tx,
                                &mut actors,
                                &updates_tx,
                            );
                        }
                    }
                }
                command = commands_rx.recv() => {
                    let Some(ManagerCommand::Session { session_id, reply }) = command else {
                        break;
                    };
                    let handle = actors.get(&session_id).map(|actor| ManagedSessionHandle {
                        session_id: session_id.clone(),
                        commands: actor.commands.clone(),
                        releases: actor.releases.clone(),
                        view: actor.view.clone(),
                    });
                    let _ = reply.send(handle);
                }
                published = published_rx.recv() => {
                    let Some(RemoteManagerUpdate::Publish { session_id, view }) = published else {
                        break;
                    };
                    latest.insert(session_id.clone(), view.clone());
                    if !desired.contains_key(&session_id) {
                        continue;
                    }
                    if let Some(actor) = actors.get(&session_id) {
                        publish_view(&session_id, view, &actor.view_tx, &updates_tx);
                        continue;
                    }
                    spawn_remote_actor(
                        session_id,
                        view,
                        &requests_tx,
                        &mut actors,
                        &updates_tx,
                    );
                }
            }
        }
        for actor in actors.into_values() {
            actor.abort.abort();
        }
    });
    Ok(RemoteSessionManagerChannels {
        targets: targets_tx,
        control: SessionManagerControl {
            commands: commands_tx,
        },
        updates: updates_rx,
        shutdown: SessionManagerShutdown {
            signal: Some(shutdown_tx),
            task: Some(task),
        },
        publisher: RemoteSessionPublisher {
            updates: published_tx,
        },
        requests: RemoteSessionRequests {
            requests: requests_rx,
        },
    })
}

pub fn spawn_session_manager() -> Result<SessionManagerChannels> {
    let (targets_tx, mut targets_rx) = watch::channel(Vec::<RelaySessionTarget>::new());
    let (commands_tx, mut commands_rx) = mpsc::channel(32);
    let (updates_tx, updates_rx) = coalesced_update_channel();
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut actors = BTreeMap::<String, ActorRegistration>::new();
        let mut tasks = tokio::task::JoinSet::<String>::new();
        let mut desired_targets = BTreeMap::<String, RelaySessionTarget>::new();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    desired_targets = target_map(&targets_rx.borrow_and_update());
                    reconcile_actors(
                        &desired_targets,
                        &mut actors,
                        &mut tasks,
                        &updates_tx,
                    );
                }
                command = commands_rx.recv() => {
                    let Some(ManagerCommand::Session { session_id, reply }) = command else {
                        break;
                    };
                    let handle = actors
                        .get(&session_id)
                        .filter(|actor| !actor.commands.is_closed())
                        .filter(|actor| desired_targets.get(&session_id) == Some(&actor.target))
                        .map(|actor| ManagedSessionHandle {
                            session_id: session_id.clone(),
                            commands: actor.commands.clone(),
                            releases: actor.releases.clone(),
                            view: actor.view.clone(),
                        });
                    if reply.send(handle).is_err() {
                        tracing::debug!(
                            session_id = %session_id,
                            operation = "session_lookup",
                            "session lookup receiver was already closed"
                        );
                    }
                }
                joined = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    match joined {
                        Some(Ok((task_id, session_id))) => {
                            let removed = remove_actor_task(&mut actors, task_id);
                            if removed.as_deref().is_some_and(|removed| removed != session_id) {
                                tracing::error!(
                                    completed_session_id = session_id,
                                    registered_session_id = removed,
                                    "session relay actor completed under the wrong registration"
                                );
                            }
                            // A watch sender may have published another target while this
                            // completion was already ready. Reconcile against its newest
                            // value so an intermediate replacement is never started.
                            desired_targets = target_map(&targets_rx.borrow());
                            reconcile_actors(
                                &desired_targets,
                                &mut actors,
                                &mut tasks,
                                &updates_tx,
                            );
                        }
                        Some(Err(error)) if error.is_cancelled() => {
                            let cancelled_task = error.id();
                            let session_id = remove_actor_task(&mut actors, cancelled_task);
                            desired_targets = target_map(&targets_rx.borrow());
                            reconcile_actors(
                                &desired_targets,
                                &mut actors,
                                &mut tasks,
                                &updates_tx,
                            );
                            tracing::warn!(
                                session_id = ?session_id,
                                "cancelled session relay actor was replaced"
                            );
                        }
                        Some(Err(error)) => {
                            let failed_task = error.id();
                            remove_actor_task(&mut actors, failed_task);
                            desired_targets = target_map(&targets_rx.borrow());
                            reconcile_actors(
                                &desired_targets,
                                &mut actors,
                                &mut tasks,
                                &updates_tx,
                            );
                            tracing::error!(%error, "session relay actor failed");
                        }
                        None => {}
                    }
                }
            }
        }
        shutdown_session_actors(&mut actors, &mut tasks).await;
    });
    Ok(SessionManagerChannels {
        targets: targets_tx,
        control: SessionManagerControl {
            commands: commands_tx,
        },
        updates: updates_rx,
        shutdown: SessionManagerShutdown {
            signal: Some(shutdown_tx),
            task: Some(task),
        },
    })
}

async fn shutdown_session_actors(
    actors: &mut BTreeMap<String, ActorRegistration>,
    tasks: &mut tokio::task::JoinSet<String>,
) {
    for actor in actors.values() {
        actor.retirement.send_replace(true);
    }
    actors.clear();

    let graceful = async {
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(_) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    tracing::error!(%error, "session relay actor failed during shutdown");
                }
            }
        }
    };
    if tokio::time::timeout(SESSION_MANAGER_SHUTDOWN_GRACE, graceful)
        .await
        .is_ok()
    {
        return;
    }

    tracing::warn!(
        timeout_ms = SESSION_MANAGER_SHUTDOWN_GRACE.as_millis(),
        "session relay actors did not stop before the shutdown deadline; aborting them"
    );
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined
            && !error.is_cancelled()
        {
            tracing::error!(%error, "session relay actor failed while being aborted");
        }
    }
}

async fn run_session_actor(
    target: RelaySessionTarget,
    mut commands: mpsc::Receiver<ActorCommand>,
    mut releases: mpsc::UnboundedReceiver<ReturnedConnection>,
    mut retirement: watch::Receiver<bool>,
    view_tx: watch::Sender<ManagedSessionView>,
    updates: CoalescedUpdateSender,
) {
    let mut connection: Option<StandaloneSession> = None;
    let mut failures = 0_u32;
    let mut last_recovery_probe = None;
    let mut lifecycle = ActorLifecycle::default();
    let mut deferred_submits: VecDeque<DeferredSubmit> = VecDeque::new();
    let mut next_lease_id = 1_u64;
    let mut reviewer_tasks = tokio::task::JoinSet::new();
    let mut reviewer_connections = BTreeMap::new();
    let mut reviewer_tails = BTreeMap::new();
    let mut reviewer_cancellation = tokio_util::sync::CancellationToken::new();
    let mut interval = tokio::time::interval(SESSION_SYNC_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        lifecycle.set_retirement_requested(*retirement.borrow_and_update());
        if lifecycle.should_stop() {
            break;
        }
        tokio::select! {
            completed = reviewer_tasks.join_next(), if !reviewer_tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::error!(session_id = %target.session_id, %error, "reviewer operation task failed");
                }
            }
            _ = interval.tick() => {
                lifecycle.set_retirement_requested(*retirement.borrow());
                if lifecycle.should_stop() {
                    break;
                }
                if lifecycle.is_leased() {
                    continue;
                }
                let result = sync_actor_connection(
                    &target,
                    &mut connection,
                ).await;
                match result {
                    Ok(snapshot) => {
                        failures = 0;
                        if let Some(snapshot) = snapshot {
                            publish_view(&target.session_id, ManagedSessionView {
                                snapshot: Some(snapshot),
                                connected: true,
                                error: None,
                            }, &view_tx, &updates);
                        }
                    }
                    Err(error) => {
                        connection = None;
                        failures = failures.saturating_add(1);
                        // A projection integrity failure repeats on every
                        // retry, so report it at once rather than waiting for
                        // the unreachable threshold.
                        let integrity = projection_integrity_failure(&error);
                        tracing::warn!(
                            session_id = target.session_id,
                            consecutive_failures = failures,
                            projection_integrity = integrity,
                            transport_dead = worker_connect_needs_restart(&error),
                            "session relay sync failed: {error:#}"
                        );
                        let recovery_due = !integrity
                            && !crate::controller::move_session::move_owns_session(&target.session_id)
                            && failures >= UNREACHABLE_FAILURE_THRESHOLD
                            && worker_connect_needs_restart(&error)
                            && target.worker_recovery.is_some()
                            && last_recovery_probe.is_none_or(|last: tokio::time::Instant| {
                                last.elapsed() >= WORKER_RESTART_COOLDOWN
                            });
                        if integrity || failures >= UNREACHABLE_FAILURE_THRESHOLD {
                            // Bind the clone first: borrowing inside the call
                            // would hold the watch read guard while
                            // `publish_view` takes the write lock, deadlocking
                            // this actor on its own view.
                            let snapshot = view_tx.borrow().snapshot.clone();
                            let mut detail = format!("{error:#}");
                            if recovery_due {
                                detail.push_str("; checking whether the relay worker is dead");
                            }
                            publish_view(&target.session_id, ManagedSessionView {
                                snapshot,
                                connected: false,
                                error: Some(if integrity {
                                    ViewError::ProjectionIntegrity(detail)
                                } else {
                                    ViewError::Unreachable(detail)
                                }),
                            }, &view_tx, &updates);
                        }
                        if recovery_due {
                            last_recovery_probe = Some(tokio::time::Instant::now());
                            let plan = target
                                .worker_recovery
                                .clone()
                                .expect("recovery eligibility requires a plan");
                            let restart_unresponsive =
                                worker_connect_allows_live_restart(&error);
                            tracing::warn!(
                                session_id = target.session_id,
                                "relay worker is unreachable; probing it before recovery: {error:#}"
                            );
                            match recover_worker_for_session(plan, restart_unresponsive, Some(target.session_id.clone())).await {
                                Ok(
                                    outcome @ (WorkerRecoveryOutcome::RestartedDead
                                    | WorkerRecoveryOutcome::RestartedUnresponsive),
                                ) => {
                                    failures = 0;
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    let recovery = match outcome {
                                        WorkerRecoveryOutcome::RestartedDead => {
                                            "confirmed the relay worker was dead and restarted it"
                                        }
                                        WorkerRecoveryOutcome::RestartedUnresponsive => {
                                            "the relay worker was alive but not serving handshakes, so it was restarted"
                                        }
                                        WorkerRecoveryOutcome::Alive
                                        | WorkerRecoveryOutcome::Starting
                                        | WorkerRecoveryOutcome::TargetMissing
                                        | WorkerRecoveryOutcome::Suppressed
                                        | WorkerRecoveryOutcome::WorkspaceMissing(_) => {
                                            unreachable!()
                                        }
                                    };
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; {recovery}"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(RECONNECT_INTERVAL);
                                }
                                Ok(WorkerRecoveryOutcome::Alive) => {
                                    tracing::warn!(
                                        session_id = target.session_id,
                                        "relay transport failed but the worker is alive; leaving it running"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; relay worker is still alive, so it was not restarted"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::Starting) => {
                                    tracing::warn!(
                                        session_id = target.session_id,
                                        "relay worker is still starting; leaving it running"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; relay worker is still recovering its durable state, so it was not restarted"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(reconnect_delay(failures));
                                }
                                Ok(WorkerRecoveryOutcome::Suppressed) => {
                                    tracing::info!(
                                        session_id = target.session_id,
                                        "automatic worker recovery suppressed by durable lifecycle or target change"
                                    );
                                    // The desired-target refresher will remove or replace this
                                    // stale actor. It must not reconnect in the meantime.
                                    break;
                                }
                                Ok(WorkerRecoveryOutcome::TargetMissing) => {
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::TargetMissing(
                                            "the managed Podman session container no longer exists"
                                                .into(),
                                        )),
                                    }, &view_tx, &updates);
                                    interval.reset_after(RECONNECT_BACKOFF_CEILING);
                                }
                                Ok(WorkerRecoveryOutcome::WorkspaceMissing(directory)) => {
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::TargetMissing(format!(
                                            "the worker working directory {} is missing; resume this session from its recovery archive to restore it",
                                            directory.display(),
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(RECONNECT_BACKOFF_CEILING);
                                }
                                Err(recovery_error) => {
                                    tracing::warn!(
                                        session_id = target.session_id,
                                        "automatic relay worker recovery failed safely: {recovery_error:#}"
                                    );
                                    let snapshot = view_tx.borrow().snapshot.clone();
                                    publish_view(&target.session_id, ManagedSessionView {
                                        snapshot,
                                        connected: false,
                                        error: Some(ViewError::Unreachable(format!(
                                            "{error:#}; could not confirm the relay worker was dead, so it was not restarted: {recovery_error:#}"
                                        ))),
                                    }, &view_tx, &updates);
                                    interval.reset_after(reconnect_delay(failures));
                                }
                            }
                        } else {
                            interval.reset_after(reconnect_delay(failures));
                        }
                    }
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                lifecycle.set_retirement_requested(*retirement.borrow());
                if !lifecycle.accepts_new_work() {
                    tracing::debug!(
                        session_id = %target.session_id,
                        operation = command.operation_name(),
                        "rejecting relay operation while session target changes"
                    );
                    command.reject(&target.session_id, "session target is changing");
                    continue;
                }
                match command {
                    ActorCommand::Submit {
                        command_id,
                        command,
                        admission,
                        reply,
                    } => {
                        if crate::controller::move_session::move_refuses_command(&target.session_id, &command) {
                            let _ = reply.send(Err("session is moving; keep the draft and retry after Move finishes".into()));
                            continue;
                        }
                        // A turn under review holds its session's prompts. The
                        // sole exception is a capability issued by the review
                        // host for this exact corrective command; ordinary
                        // prompts and controller-authored notices still take
                        // the refusal path below.
                        let admitted = admission.as_ref().is_some_and(|admission| {
                            matches!(&command, RelayCommand::Prompt { .. })
                                && admission.command_id() == command_id
                                && crate::review_host::review_delivery_admitted(
                                    &target.session_id,
                                    admission,
                                )
                        });
                        if admission.is_some() && !admitted {
                            let _ = reply.send(Err(
                                "review delivery admission is no longer valid".to_owned(),
                            ));
                            continue;
                        }
                        if matches!(&command, RelayCommand::Prompt { .. })
                            && !admitted
                            && let Some(refusal) =
                                crate::review_host::prompt_refusal(&target.session_id)
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                %command_id,
                                "refusing a prompt while a turn review is unresolved"
                            );
                            let _ = reply.send(Err(refusal.to_owned()));
                            continue;
                        }
                        if lifecycle.is_leased() {
                            // A checkpoint or other lifecycle operation owns the
                            // connection. Hold the prompt instead of rejecting it
                            // and deliver it when the lease comes back.
                            deferred_submits.push_back(DeferredSubmit {
                                command_id,
                                command,
                                admission,
                                reply,
                            });
                            continue;
                        }
                        deliver_submit(
                            &target,
                            &mut connection,
                            DeferredSubmit { command_id, command, admission, reply },
                            &view_tx,
                            &updates,
                        )
                        .await;
                    }
                    ActorCommand::Sync { reply } => {
                        if lifecycle.is_leased() {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "sync",
                                "rejecting sync while session is leased"
                            );
                            if reply
                                .send(Err("session is reserved for a lifecycle operation".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "sync",
                                    "sync rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        let result = sync_actor_connection(
                            &target,
                            &mut connection,
                        ).await.map(|snapshot| {
                            if let Some(snapshot) = snapshot {
                                publish_view(&target.session_id, ManagedSessionView {
                                    snapshot: Some(snapshot),
                                    connected: true,
                                    error: None,
                                }, &view_tx, &updates);
                            }
                        });
                        if result.is_err() {
                            connection = None;
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "sync",
                                error = %error,
                                "explicit relay synchronization failed"
                            );
                        }
                    if reply.send(result.map_err(|error| format!("{error:#}"))).is_err() {
                        tracing::debug!(
                            session_id = %target.session_id,
                            operation = "sync",
                            "sync result receiver was already closed"
                        );
                    }
                    }
                    ActorCommand::Reviewer {
                        role,
                        action,
                        reply,
                    } => {
                        if lifecycle.is_leased() || crate::controller::move_session::move_owns_session(&target.session_id) {
                            // A lifecycle operation owns the connection, and a
                            // reviewer action is not worth deferring: the user
                            // is waiting on its answer now.
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = action.operation_name(),
                                "rejecting a reviewer action while the session is leased"
                            );
                            if reply
                                .send(Err("session is reserved for a lifecycle operation".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "reviewer",
                                    "reviewer rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        // A slow harness startup or analysis must not occupy
                        // the primary's relay or serialize independent roles.
                        // Cache each role's connection so transcript polling
                        // does not launch a new SSH/Podman proxy every time.
                        let cached = reviewer_connections.entry(role.clone())
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
                            .clone();
                        let (finished, tail) = oneshot::channel::<()>();
                        let previous = reviewer_tails.insert(role.clone(), tail);
                        let target = target.clone();
                        let cancelled = reviewer_cancellation.clone();
                        reviewer_tasks.spawn(async move {
                            if let Some(previous) = previous {
                                let _ = previous.await;
                            }
                            run_reviewer_operation(target, role, action, reply, cached, cancelled).await;
                            drop(finished);
                        });
                    }
                    ActorCommand::RespondElicitation {
                        elicitation_id,
                        response,
                        reply,
                    } => {
                        if lifecycle.is_leased() {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "respond_elicitation",
                                "rejecting elicitation response while session is leased"
                            );
                            if reply
                                .send(Err("session is reserved for a lifecycle operation".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "respond_elicitation",
                                    "elicitation rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        let result = async {
                            sync_actor_connection(&target, &mut connection).await?;
                            let connection = connection
                                .as_mut()
                                .context("relay is disconnected")?;
                            connection
                                .respond_elicitation(elicitation_id, response)
                                .await?;
                            Ok::<_, anyhow::Error>(connection.snapshot())
                        }
                        .await;
                        match result {
                            Ok(ref snapshot) => publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    snapshot: Some(snapshot.clone()),
                                    connected: true,
                                    error: None,
                                },
                                &view_tx,
                                &updates,
                            ),
                            Err(ref error) if !is_final_rejection(error) => connection = None,
                            Err(_) => {}
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "respond_elicitation",
                                error = %error,
                                "relay elicitation response failed"
                            );
                        }
                        if reply
                            .send(result.map(|_| ()).map_err(|error| format!("{error:#}")))
                            .is_err()
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "respond_elicitation",
                                "elicitation result receiver was already closed"
                            );
                        }
                    }
                    ActorCommand::StopBackgroundTask {
                        background_task_id,
                        reply,
                    } => {
                        if lifecycle.is_leased() {
                            let _ = reply.send(Err(
                                "session is reserved for a lifecycle operation".into(),
                            ));
                            continue;
                        }
                        let result = async {
                            sync_actor_connection(&target, &mut connection).await?;
                            let connection = connection
                                .as_mut()
                                .context("relay is disconnected")?;
                            connection.stop_background_task(background_task_id).await?;
                            Ok::<_, anyhow::Error>(connection.snapshot())
                        }
                        .await;
                        match result {
                            Ok(ref snapshot) => publish_view(
                                &target.session_id,
                                ManagedSessionView {
                                    snapshot: Some(snapshot.clone()),
                                    connected: true,
                                    error: None,
                                },
                                &view_tx,
                                &updates,
                            ),
                            Err(ref error) if !is_final_rejection(error) => connection = None,
                            Err(_) => {}
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "stop_background_task",
                                error = %error,
                                "relay background task stop failed"
                            );
                        }
                        if reply
                            .send(result.map(|_| ()).map_err(|error| format!("{error:#}")))
                            .is_err()
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "stop_background_task",
                                "background task stop result receiver was already closed"
                            );
                        }
                    }
                    ActorCommand::Lease { reply } => {
                        if lifecycle.is_leased() {
                            tracing::debug!(
                                session_id = %target.session_id,
                                operation = "lease",
                                "rejecting duplicate session lifecycle lease"
                            );
                            if reply
                                .send(Err(anyhow::anyhow!(
                                    "session already has a lifecycle operation"
                                )))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "lease",
                                    "lease rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        let lease_id = next_lease_id;
                        reviewer_cancellation.cancel();
                        reviewer_cancellation = tokio_util::sync::CancellationToken::new();
                        reviewer_connections.clear();
                        reviewer_tails.clear();
                        let result = sync_actor_connection(
                            &target,
                            &mut connection,
                        )
                        .await
                        .map(|_| {
                            next_lease_id = next_lease_id.wrapping_add(1).max(1);
                            (
                                lease_id,
                                connection
                                    .take()
                                    .expect("successful sync retained its connection"),
                            )
                        });
                        if result.is_err() {
                            connection = None;
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                operation = "lease",
                                error = %error,
                                "could not acquire relay session lease"
                            );
                        }
                        let acquired = result.is_ok();
                        match reply.send(result) {
                            Ok(()) if acquired => lifecycle.activate_lease(lease_id),
                            Ok(()) => {}
                            Err(Ok((_lease_id, returned))) => connection = Some(returned),
                            Err(Err(_)) => {}
                        }
                    }
                }
            }
            returned = releases.recv() => {
                let Some(returned) = returned else { continue };
                if lifecycle.return_lease(returned.lease_id) {
                    // A dropped lease returns no connection; `submit_actor_command`
                    // reconnects on demand, so the drain needs no special case.
                    connection = returned.connection;
                    failures = 0;
                    interval.reset();
                    // A lease syncs the connection it borrowed, so this actor's
                    // next sync can find nothing left to apply. Publish what the
                    // returned connection already knows or watchers keep reading
                    // pre-lease state.
                    if let Some(returned) = connection.as_ref() {
                        publish_view(&target.session_id, ManagedSessionView {
                            snapshot: Some(returned.snapshot()),
                            connected: true,
                            error: None,
                        }, &view_tx, &updates);
                    }
                    let retiring = *retirement.borrow();
                    while let Some(deferred) = deferred_submits.pop_front() {
                        if retiring {
                            if deferred
                                .reply
                                .send(Err("session target is changing".into()))
                                .is_err()
                            {
                                tracing::debug!(
                                    session_id = %target.session_id,
                                    operation = "submit",
                                    "deferred submit rejection receiver was already closed"
                                );
                            }
                            continue;
                        }
                        deliver_submit(
                            &target,
                            &mut connection,
                            deferred,
                            &view_tx,
                            &updates,
                        )
                        .await;
                    }
                }
            }
            changed = retirement.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    reviewer_cancellation.cancel();
    reviewer_connections.clear();
    reviewer_tails.clear();
    while let Some(completed) = reviewer_tasks.join_next().await {
        if let Err(error) = completed {
            tracing::error!(session_id = %target.session_id, %error, "reviewer operation task failed during shutdown");
        }
    }
    if let Some(connection) = connection.take()
        && let Err(error) = connection.detach().await
    {
        tracing::warn!(
            session_id = %target.session_id,
            %error,
            "could not detach relay connection during session actor shutdown"
        );
    }
    // No caller may wait forever on a submission this actor will never deliver.
    for deferred in deferred_submits {
        if deferred
            .reply
            .send(Err("session manager stopped".into()))
            .is_err()
        {
            tracing::debug!(
                session_id = %target.session_id,
                operation = "submit",
                "deferred submit shutdown receiver was already closed"
            );
        }
    }
}

/// Submit one relay command and publish the resulting snapshot. Live and
/// deferred submissions share this path so both report identical results.
async fn deliver_submit(
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
    submission: DeferredSubmit,
    view_tx: &watch::Sender<ManagedSessionView>,
    updates: &CoalescedUpdateSender,
) {
    let DeferredSubmit {
        command_id,
        command,
        admission,
        reply,
    } = submission;
    if crate::controller::move_session::move_refuses_command(&target.session_id, &command) {
        let _ = reply.send(Err(
            "session is moving; keep the draft and retry after Move finishes".into(),
        ));
        return;
    }
    if let Some(admission) = admission.as_ref()
        && (!matches!(&command, RelayCommand::Prompt { .. })
            || admission.command_id() != command_id
            || !crate::review_host::review_delivery_admitted(&target.session_id, admission))
    {
        let _ = reply.send(Err(
            "review delivery admission is no longer valid".to_owned()
        ));
        return;
    }
    let result = submit_actor_command(target, connection, &command_id, &command).await;
    if let Err(error) = result.as_ref() {
        tracing::warn!(
            session_id = %target.session_id,
            operation = "submit",
            %command_id,
            retryable = !is_final_rejection(error),
            error = %error,
            "relay command submission failed"
        );
    }
    if let Err(error) = result.as_ref()
        && !is_final_rejection(error)
    {
        *connection = None;
    }
    let accepted = result.as_ref().ok().copied();
    // Answer the caller the moment the relay has the command. Catching the
    // local projection up to it is the expensive half and nobody waiting to
    // hear "accepted" needs it first: the caller has an ordinal, and the view
    // it would read is published below anyway.
    if reply
        .send(result.map_err(|error| format!("{error:#}")))
        .is_err()
    {
        tracing::debug!(
            session_id = %target.session_id,
            operation = "submit",
            %command_id,
            "submit result receiver was already closed"
        );
    }
    let Some(ordinal) = accepted else {
        return;
    };
    tracing::trace!(%ordinal, %command_id, "relay command accepted");
    let Some(session) = connection.as_mut() else {
        return;
    };
    // The command landed either way, so a failed catch-up is a connection
    // problem to retire rather than a failed submission: the caller has
    // already been told the relay took it.
    match session.sync().await {
        Ok(snapshot) => publish_view(
            &target.session_id,
            ManagedSessionView {
                snapshot: Some(snapshot),
                connected: true,
                error: None,
            },
            view_tx,
            updates,
        ),
        Err(error) => {
            tracing::warn!(
                session_id = %target.session_id,
                operation = "submit",
                %command_id,
                error = %format!("{error:#}"),
                "projection could not catch up to an accepted command"
            );
            *connection = None;
        }
    }
}

/// Whether the relay refused this request outright.
///
/// A refusal is a completed round trip, so the connection is healthy. Dropping
/// it would discard whatever that connection owns on the worker, including a
/// checkpoint barrier a controller is still holding.
fn is_final_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RelayRejected>()
        .is_some_and(|rejected| !rejected.is_retryable())
}

async fn submit_actor_command(
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
    command_id: &str,
    command: &RelayCommand,
) -> Result<u64> {
    let mut first_error = None;
    for attempt in 1..=2 {
        if connection.is_none() {
            sync_actor_connection(target, connection).await?;
        }
        let result = connection
            .as_mut()
            .context("relay is disconnected")?
            .submit_accepted(command_id.to_owned(), command.clone())
            .await;
        match result {
            Ok(ordinal) => return Ok(ordinal),
            // A final rejection is a completed round trip: the relay read the
            // command and refused it, so retrying would only be refused again.
            // Reconnecting would also cancel any checkpoint barrier this
            // connection owns, which is how a controller probing for a command
            // an older worker does not understand would lose it.
            Err(error) if is_final_rejection(&error) => return Err(error),
            Err(error) => {
                tracing::warn!(
                    session_id = %target.session_id,
                    operation = "submit",
                    %command_id,
                    attempt,
                    retryable = true,
                    error = %error,
                    "retryable relay command failure; reconnecting"
                );
                if first_error.is_none() {
                    first_error = Some(format!("{error:#}"));
                }
                *connection = None;
            }
        }
    }
    let detail = first_error.unwrap_or_else(|| "relay submission failed".into());
    bail!("relay command {command_id} failed after an idempotent reconnect: {detail}")
}

/// Perform one reviewer action on a synchronized relay connection.
///
/// The reviewer's own relay answers most of these, so the outcomes mirror the
/// primary's: an attach page, an acknowledgement cursor, an accepted command.
async fn run_reviewer_operation(
    target: RelaySessionTarget,
    role: Option<String>,
    action: ReviewerAction,
    mut reply: oneshot::Sender<std::result::Result<ReviewerOutcome, String>>,
    cached: Arc<tokio::sync::Mutex<Option<RelayClient>>>,
    cancelled: tokio_util::sync::CancellationToken,
) {
    let operation = action.operation_name();
    let keep_connection = !matches!(&action, ReviewerAction::Pause);
    let result = tokio::select! {
        biased;
        _ = cancelled.cancelled() => Err(anyhow::anyhow!("reviewer operation cancelled for session lifecycle change")),
        _ = reply.closed() => return,
        result = async {
            let mut cache = cached.lock().await;
            // Take ownership while a request is in flight: dropping this
            // future closes its connection instead of leaving a late reply
            // available for the next request to misinterpret.
            let mut client = match cache.take() {
                Some(client) => client,
                None => RelayClient::connect(&target.spec, &target.session_id).await?,
            };
            let result = drive_reviewer(&mut client, role, action).await;
            if keep_connection && (result.is_ok() || result.as_ref().is_err_and(is_final_rejection)) {
                *cache = Some(client);
            }
            result
        } => result,
    };
    if let Err(error) = &result {
        tracing::warn!(session_id = %target.session_id, %operation, error = %error, "reviewer action failed");
    }
    if reply
        .send(result.map_err(|error| format!("{error:#}")))
        .is_err()
    {
        tracing::debug!(session_id = %target.session_id, %operation, "reviewer result receiver was already closed");
    }
}

async fn drive_reviewer(
    client: &mut RelayClient,
    role: Option<String>,
    action: ReviewerAction,
) -> Result<ReviewerOutcome> {
    let role = role.as_deref();
    Ok(match action {
        ReviewerAction::Start { config } => {
            ReviewerOutcome::Started(Box::new(client.start_reviewer(role, *config).await?))
        }
        ReviewerAction::Submit {
            command_id,
            command,
        } => ReviewerOutcome::Accepted {
            ordinal: client.submit_to_reviewer(role, command_id, command).await?,
        },
        ReviewerAction::Attach {
            after_ordinal,
            after_digest,
        } => ReviewerOutcome::Attached(Box::new(
            client
                .attach_reviewer(role, after_ordinal, after_digest)
                .await?,
        )),
        ReviewerAction::Acknowledge {
            through_ordinal,
            through_digest,
        } => ReviewerOutcome::Acknowledged(
            client
                .acknowledge_reviewer(role, through_ordinal, through_digest)
                .await?,
        ),
        ReviewerAction::Status => {
            ReviewerOutcome::Status(Box::new(client.reviewer_status(role).await?))
        }
        ReviewerAction::RespondElicitation {
            elicitation_id,
            response,
        } => {
            client
                .respond_to_reviewer(role, elicitation_id, response)
                .await?;
            ReviewerOutcome::ElicitationResolved
        }
        ReviewerAction::Pause => {
            client.pause_reviewer(role).await?;
            ReviewerOutcome::Paused
        }
        ReviewerAction::CaptureDelta { baselines } => ReviewerOutcome::Delta {
            repositories: client.capture_review_delta(role, baselines).await?,
        },
        ReviewerAction::AdvanceBaseline { trees } => {
            client.advance_review_baseline(role, trees).await?;
            ReviewerOutcome::BaselineAdvanced
        }
        ReviewerAction::AnalyzeDelta { repositories } => ReviewerOutcome::ChangedFunctions {
            packet: client.analyze_review_delta(role, repositories).await?,
        },
        ReviewerAction::TakeLaneDispatches => ReviewerOutcome::LaneDispatches {
            requests: client.take_lane_dispatches().await?,
        },
    })
}

async fn sync_actor_connection(
    target: &RelaySessionTarget,
    connection: &mut Option<StandaloneSession>,
) -> Result<Option<ManagedSessionSnapshot>> {
    if connection.is_none() {
        let fresh = StandaloneSession::connect(target).await?;
        let snapshot = fresh.snapshot();
        *connection = Some(fresh);
        return Ok(Some(snapshot));
    }
    let connection = connection.as_mut().expect("connection was initialized");
    if connection.sync_in_place().await? {
        Ok(Some(connection.snapshot()))
    } else {
        Ok(None)
    }
}

/// Cheap equivalence for published views.
///
/// The materialized projection is a function of the relay event chain, so its
/// transcript can only differ when the applied event frontier differs. Every
/// sync tick would otherwise walk the whole conversation to prove nothing
/// changed. The remaining scalars are compared directly because they are small
/// and bound the projection's non-transcript state.
fn view_is_unchanged(current: &ManagedSessionView, next: &ManagedSessionView) -> bool {
    if current.connected != next.connected || current.error != next.error {
        return false;
    }
    match (&current.snapshot, &next.snapshot) {
        (None, None) => true,
        (Some(current), Some(next)) => {
            let (current_session, next_session) = (&current.materialized, &next.materialized);
            current.latest_credential_sync_signal == next.latest_credential_sync_signal
                && current.operational == next.operational
                // Sub-agent requests/results are non-transcript projection state:
                // they come from the separate `subagents.json` poll in
                // `sync_in_place`, not the relay event chain, so they can change
                // while every transcript scalar below stays identical. They must
                // be compared here, or a request that lands without a coincident
                // view change (e.g. one that survives a daemon restart, where the
                // tool-call ordinal is already applied) is never republished to the
                // drain and its `serve_one` waits to the socket ceiling.
                && current.subagent_requests == next.subagent_requests
                && current.subagent_results == next.subagent_results
                && current_session.session_id == next_session.session_id
                && current_session.applied_event_ordinal == next_session.applied_event_ordinal
                && current_session.applied_event_digest == next_session.applied_event_digest
                && current_session.last_activity_at_ms == next_session.last_activity_at_ms
                && current_session.execution == next_session.execution
                && current_session.session_title == next_session.session_title
                && current_session.queued_prompts == next_session.queued_prompts
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn publish_view(
    session_id: &str,
    view: ManagedSessionView,
    watch: &watch::Sender<ManagedSessionView>,
    updates: &CoalescedUpdateSender,
) {
    // Compare and replace under one lock acquisition; a separate
    // `watch.borrow()` check would reacquire the lock and invite the
    // read-then-write deadlock this function's callers must avoid.
    let changed = watch.send_if_modified(|current| {
        if view_is_unchanged(current, &view) {
            return false;
        }
        *current = view.clone();
        true
    });
    if changed {
        updates.send(SessionManagerUpdate {
            session_id: session_id.to_owned(),
            view,
        });
    }
}

/// Read a stored projection without blocking the runtime. The rusqlite read
/// and the transcript deserialization behind it are synchronous and grow with
/// the conversation, so a long session must not stall a worker thread that
/// other actors share.
async fn load_projection(session_id: &str) -> Result<MaterializedSession> {
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || -> Result<MaterializedSession> {
        let loaded = crate::database::load_materialized_session(&session_id)?;
        Ok(loaded.unwrap_or_else(|| MaterializedSession::empty(session_id)))
    })
    .await
    .context("controller projection load task failed")?
}

pub struct StandaloneSession {
    client: RelayClient,
    materialized: MaterializedSession,
    operational: RelayOperationalState,
    latest_credential_sync_signal: Option<CredentialSyncSignal>,
    project_memory: Option<ProjectMemorySyncTarget>,
    subagent_requests: Vec<mj_core::subagent::SubagentToolRequest>,
    subagent_results: Vec<mj_core::subagent::SubagentToolResult>,
}

impl StandaloneSession {
    pub fn set_project_memory_target(&mut self, target: Option<ProjectMemorySyncTarget>) {
        self.project_memory = target;
    }

    pub async fn connect(target: &RelaySessionTarget) -> Result<Self> {
        // Reach the worker before reading the projection. A stored session can
        // be tens of megabytes, and the reconnect loop would otherwise pay that
        // whole synchronous read on every attempt against a worker that is down.
        let mut client = RelayClient::connect(&target.spec, &target.session_id).await?;
        let operational = client.status().await?;
        let materialized = load_projection(&target.session_id).await?;
        let mut connection = Self {
            client,
            materialized,
            operational,
            latest_credential_sync_signal: None,
            project_memory: target.project_memory.clone(),
            subagent_requests: Vec::new(),
            subagent_results: Vec::new(),
        };
        connection.sync_in_place().await?;
        Ok(connection)
    }

    pub async fn connect_command(spec: &CommandSpec, session_id: &str) -> Result<Self> {
        Self::connect(&RelaySessionTarget {
            session_id: session_id.to_owned(),
            spec: spec.clone(),
            worker_recovery: None,
            project_memory: None,
        })
        .await
    }

    /// Protocol negotiated with the worker behind this connection. Lifecycle
    /// operations use it to avoid sending a newly introduced command to an
    /// older worker that cannot decode it.
    pub fn protocol_version(&self) -> u32 {
        self.client.protocol_version()
    }

    async fn detach(self) -> Result<()> {
        self.client.detach().await
    }

    pub async fn sync(&mut self) -> Result<ManagedSessionSnapshot> {
        self.sync_in_place().await?;
        Ok(self.snapshot())
    }

    async fn sync_in_place(&mut self) -> Result<bool> {
        let original_ordinal = self.materialized.applied_event_ordinal;
        let original_digest = self.materialized.applied_event_digest.clone();
        let original_operational = self.operational.clone();
        let mut repaired = false;
        let mut repaired_frontiers = std::collections::HashSet::new();
        loop {
            let after_ordinal = self.materialized.applied_event_ordinal;
            match self.catch_up_fixed_frontier().await {
                Ok(()) => break,
                Err(error) if error.downcast_ref::<ProjectionAdvancedError>().is_some() => {
                    let durable = load_projection(&self.materialized.session_id).await?;
                    if durable.applied_event_ordinal <= after_ordinal {
                        return Err(error);
                    }
                    self.materialized = durable;
                    continue;
                }
                Err(error) if relay_desynchronized(&error) => {
                    self.repair_projection()
                        .await
                        .with_context(|| {
                            format!(
                                "controller projection for {} cannot catch up from ordinal {after_ordinal}: {error:#}",
                                self.materialized.session_id
                            )
                        })?;
                    repaired = true;
                    // Repair rebuilds from the same durable checkpoint every
                    // time. If catching up from that frontier still desyncs — as
                    // it does when relay history is unreadable past the
                    // checkpoint — repairing again lands on the same frontier and
                    // would loop forever. Fail loudly on the second visit instead
                    // of hanging; recovery got everything the checkpoint covers.
                    let frontier = self.materialized.applied_event_ordinal;
                    if !repaired_frontiers.insert(frontier) {
                        bail!(
                            "controller projection for {} cannot catch up: relay history is \
                             unreadable and rebuilding from checkpoint frontier {frontier} does \
                             not get past it",
                            self.materialized.session_id
                        );
                    }
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        let previous_requests = self.subagent_requests.clone();
        let previous_results = self.subagent_results.clone();
        (self.subagent_requests, self.subagent_results) = self.client.subagent_requests().await?;
        let changed = repaired
            || self.materialized.applied_event_ordinal != original_ordinal
            || self.materialized.applied_event_digest != original_digest
            || self.operational != original_operational
            || self.subagent_requests != previous_requests
            || self.subagent_results != previous_results;
        Ok(changed)
    }

    /// Apply relay pages through the exact frontier captured by the first
    /// response, then acknowledge that frontier once. Every projection page is
    /// independently durable; delaying the relay's GC watermark avoids one
    /// snapshot fsync per transport-sized page without risking redelivery.
    async fn catch_up_fixed_frontier(&mut self) -> Result<()> {
        let after = RelayCursor {
            ordinal: self.materialized.applied_event_ordinal,
            digest: self.materialized.applied_event_digest.clone(),
        };
        let catch_up = self
            .client
            .begin_catch_up(after.ordinal, &after.digest)
            .await?;
        let mut cursor = self.apply_event_page(catch_up.first_page).await?;
        let mut pages_remaining = catch_up.frontier.ordinal.saturating_sub(cursor.ordinal);
        while cursor.ordinal < catch_up.frontier.ordinal {
            ensure!(
                pages_remaining > 0,
                "relay catch-up exceeded its fixed page bound"
            );
            pages_remaining -= 1;
            let page = self
                .client
                .next_catch_up_page(&cursor, &catch_up.frontier)
                .await?;
            cursor = self.apply_event_page(page).await?;
        }
        ensure!(
            cursor == catch_up.frontier,
            "controller projection did not reach the captured relay frontier"
        );
        if cursor.ordinal > 0 {
            let acknowledged = self
                .client
                .acknowledge(cursor.ordinal, &cursor.digest)
                .await?;
            ensure!(
                acknowledged == cursor,
                "relay acknowledged cursor {}:{} instead of {}:{}",
                acknowledged.ordinal,
                acknowledged.digest,
                cursor.ordinal,
                cursor.digest,
            );
        }
        let mut operational = catch_up.state;
        operational.acknowledged_through = cursor.ordinal;
        operational.acknowledged_digest = cursor.digest;
        self.operational = operational;
        Ok(())
    }

    async fn repair_projection(&mut self) -> Result<()> {
        let state = crate::database::load_state()?;
        let record = state
            .sessions
            .get(&self.materialized.session_id)
            .context("controller session disappeared while repairing its projection")?;
        let Some(checkpoint) = record.checkpoint.as_ref() else {
            let replacement = MaterializedSession::empty(&self.materialized.session_id);
            self.client
                .attach(
                    replacement.applied_event_ordinal,
                    &replacement.applied_event_digest,
                )
                .await
                .context("relay cannot rebuild the projection from its genesis")?;
            save_materialized_session(&replacement)?;
            self.materialized = replacement;
            return Ok(());
        };
        let checkpoint_path = checkpoint.archive_path.clone();
        let archive = tokio::task::spawn_blocking(move || {
            verify_archive_streaming(&checkpoint_path).with_context(|| {
                format!(
                    "verify projection repair checkpoint {}",
                    checkpoint_path.display()
                )
            })
        })
        .await
        .context("projection repair archive verification task failed")??;
        ensure!(
            archive.archive_sha256 == checkpoint.sha256,
            "projection repair checkpoint checksum does not match controller metadata"
        );
        ensure!(
            archive.manifest.session.id == self.materialized.session_id,
            "projection repair checkpoint belongs to session {}, not {}",
            archive.manifest.session.id,
            self.materialized.session_id
        );
        let canonical = archive.canonical_session;
        ensure!(
            canonical.event_frontier == checkpoint.event_frontier,
            "projection repair checkpoint metadata frontier {} does not match archive frontier {}",
            checkpoint.event_frontier,
            canonical.event_frontier
        );

        // Prove that the relay recognizes this exact event-chain cursor before
        // replacing any controller state. A matching ordinal alone is not a
        // repair proof.
        self.client
            .attach(canonical.event_frontier, &canonical.event_frontier_digest)
            .await
            .context("relay rejected the verified checkpoint repair cursor")?;
        let replacement =
            materialized_session_from_canonical(&self.materialized.session_id, &canonical)?;
        save_materialized_session(&replacement)?;
        self.materialized = replacement;
        Ok(())
    }

    pub fn snapshot(&self) -> ManagedSessionSnapshot {
        ManagedSessionSnapshot {
            window: mj_core::state::ProjectionWindow::of(&self.materialized),
            materialized: self.materialized.clone(),
            operational: self.operational.clone(),
            latest_credential_sync_signal: self.latest_credential_sync_signal.clone(),
            worker_build: self.client.worker_build().map(str::to_owned),
            subagent_requests: self.subagent_requests.clone(),
            subagent_results: self.subagent_results.clone(),
        }
    }

    pub async fn complete_subagent_request(
        &mut self,
        result: mj_core::subagent::SubagentToolResult,
    ) -> Result<()> {
        self.client.complete_subagent_request(result).await?;
        (self.subagent_requests, self.subagent_results) = self.client.subagent_requests().await?;
        Ok(())
    }

    /// Hands one command to the relay and returns the ordinal it accepted it
    /// at, without catching the local projection up to it.
    ///
    /// Callers that need the projection current call [`Self::sync`] after.
    /// Keeping the two apart matters on the prompt path: the catch-up is the
    /// expensive half, and a caller waiting to hear that the relay took the
    /// command should not wait for it. It also stops a failed catch-up from
    /// looking like a failed submission to a caller that would retry.
    pub async fn submit_accepted(
        &mut self,
        command_id: String,
        command: RelayCommand,
    ) -> Result<u64> {
        self.client.submit(command_id, command).await
    }

    pub async fn submit(&mut self, command_id: String, command: RelayCommand) -> Result<u64> {
        let ordinal = self.submit_accepted(command_id, command).await?;
        self.sync_in_place().await?;
        Ok(ordinal)
    }

    pub async fn respond_elicitation(
        &mut self,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        self.client
            .respond_elicitation(elicitation_id, response)
            .await?;
        self.sync_in_place().await?;
        Ok(())
    }

    pub async fn stop_background_task(&mut self, background_task_id: String) -> Result<()> {
        self.client.stop_background_task(background_task_id).await?;
        self.sync_in_place().await?;
        Ok(())
    }

    /// Persist relay-private context for the next real prompt. It never
    /// contributes an event to the canonical projection.
    pub async fn install_prompt_context(&mut self, text: String) -> Result<()> {
        self.client.install_prompt_context(text).await
    }

    /// Apply one relay transport page in bounded durable chunks. A transport
    /// page can contain thousands of events, but SQLite has one global writer;
    /// regularly releasing it lets other session actors keep their views
    /// current. The relay GC watermark advances only after the complete page.
    async fn apply_event_page(&mut self, page: RelayEventPage) -> Result<RelayCursor> {
        for event in &page.events {
            if let mj_core::relay::RelayObservation::CommandQueued {
                command: RelayCommand::Prompt { prompt },
                ..
            } = &event.observation
            {
                for reference in mj_core::attachment::references(prompt)? {
                    if let Err(error) = self.client.cache_attachment(&reference).await {
                        // History remains readable even if a blob was lost. A
                        // later submission still verifies every image before
                        // admission, and must report missing data to the user.
                        tracing::warn!(
                            session_id = %self.materialized.session_id,
                            attachment = %reference.sha256,
                            %error,
                            "could not cache image attachment during replay"
                        );
                    }
                }
            }
        }

        let RelayEventPage {
            events,
            through_ordinal,
            through_digest,
        } = page;
        let event_count = events.len();
        let transaction_count = event_count.div_ceil(PROJECTION_TRANSACTION_EVENT_BUDGET);
        let started = Instant::now();
        for events in events.chunks(PROJECTION_TRANSACTION_EVENT_BUDGET) {
            let session_id = self.materialized.session_id.clone();
            let events = events.to_vec();
            let projection = self.materialized.clone();
            // Projection is CPU work and its durable page uses synchronous
            // SQLite. Keep both off the async actor runtime so independent
            // sessions stay responsive during each bounded catch-up chunk.
            let (projection, credential_sync_signal) = tokio::task::spawn_blocking(
                move || -> Result<(MaterializedSession, Option<CredentialSyncSignal>)> {
                    // The in-memory projection advances on a working copy and
                    // is published only once its page is durable.
                    let mut projection = projection;
                    let mut projection_index = ProjectionIndex::new(&projection);
                    let mut credential_sync_signal = None;
                    let mut prepared = Vec::with_capacity(events.len());
                    for event in &events {
                        let mutation =
                            project_relay_event_indexed(&projection, &projection_index, event)?
                                .mutation;
                        prepared.push((
                            event.ordinal,
                            event.previous_digest.clone(),
                            event.digest.clone(),
                            mutation.clone(),
                        ));
                        apply_committed_projection_event_indexed(
                            &mut projection,
                            &mut projection_index,
                            event,
                            mutation,
                        )?;
                        if let Some(reason) = relay_event_credential_sync_reason(event) {
                            credential_sync_signal = Some(CredentialSyncSignal {
                                ordinal: event.ordinal,
                                reason,
                            });
                        }
                    }
                    drop(projection_index);
                    apply_projection_page(&session_id, move |committed| {
                        for (ordinal, previous_digest, digest, mutation) in prepared {
                            match committed.apply(ordinal, &previous_digest, &digest, &mutation)? {
                                ProjectionApplyOutcome::Applied => {}
                                ProjectionApplyOutcome::AlreadyApplied => {
                                    return Err(ProjectionAdvancedError {
                                        event_ordinal: ordinal,
                                    }
                                    .into());
                                }
                            }
                        }
                        Ok((projection, credential_sync_signal))
                    })
                },
            )
            .await
            .context("relay projection page task failed")??;
            self.materialized = projection;
            if let Some(signal) = credential_sync_signal {
                self.latest_credential_sync_signal = Some(signal);
            }
        }
        if transaction_count > 1 {
            tracing::debug!(
                session_id = self.materialized.session_id,
                event_count,
                transaction_count,
                elapsed_ms = started.elapsed().as_millis(),
                "applied a large relay page in bounded projection transactions"
            );
        }
        let delivered_through = self.materialized.applied_event_ordinal;
        ensure!(
            delivered_through == through_ordinal,
            "relay page claimed frontier {} but delivered through {delivered_through}",
            through_ordinal
        );
        ensure!(
            self.materialized.applied_event_digest == through_digest,
            "relay page digest does not match its claimed frontier"
        );
        Ok(RelayCursor {
            ordinal: delivered_through,
            digest: self.materialized.applied_event_digest.clone(),
        })
    }

    /// Reconcile this worker's project-memory replica at an explicit durable
    /// boundary. Normal relay attachment and polling must never perform this
    /// filesystem work: a degraded target could otherwise turn reconnects
    /// into an unbounded queue of timed-out snapshot writes.
    pub async fn sync_project_memory(&mut self) -> Result<()> {
        let Some(target) = self.project_memory.clone() else {
            return Ok(());
        };
        if !self.client.supports_project_memory_sync() {
            tracing::warn!(
                session_id = self.materialized.session_id,
                "worker protocol predates project-memory synchronization; preserving memory through checkpoints only"
            );
            self.project_memory = None;
            return Ok(());
        }
        let (baseline, replica) = match self.client.project_memory_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error)
                if error
                    .downcast_ref::<RelayRejected>()
                    .is_some_and(|rejected| {
                        rejected.0.code == mj_core::relay::RelayErrorCode::InvalidState
                    }) =>
            {
                tracing::warn!(
                    session_id = self.materialized.session_id,
                    "worker has no project-memory endpoint; preserving memory through checkpoints only"
                );
                self.project_memory = None;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let canonical_root = target.canonical_root;
        let session_id = self.materialized.session_id.clone();
        let (reconciliation, worker_install_needed) = tokio::task::spawn_blocking(move || {
            let reconciliation = mj_core::project_memory::reconcile_into_canonical(
                &canonical_root,
                &baseline,
                &replica,
                &session_id,
            )?;
            let worker_install_needed =
                reconciliation.merged != baseline || reconciliation.merged != replica;
            Ok::<_, anyhow::Error>((reconciliation, worker_install_needed))
        })
        .await
        .context("project memory reconciliation task failed")??;
        for conflict in &reconciliation.conflicts {
            tracing::warn!(session_id = self.materialized.session_id, %conflict, "project memory conflict preserved");
        }
        if worker_install_needed {
            self.client
                .install_project_memory_snapshot(reconciliation.merged)
                .await?;
        }
        Ok(())
    }
}

fn relay_desynchronized(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<RelayRejected>()
            .is_some_and(RelayRejected::is_desynchronized)
    })
}

fn projection_integrity_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<ProjectionIntegrityError>().is_some())
}

/// A stopped actor and the manager that resolves its live replacement.
///
/// This fixture and its constructor are compiled unconditionally and hidden
/// from the documentation because the chat crate's tests need them, and a
/// `#[cfg(test)]` item is invisible to another crate.
#[cfg(test)]
struct ReplacementSessionTestFixture {
    stopped: ManagedSessionHandle,
    control: SessionManagerControl,
    submitted: mpsc::UnboundedReceiver<RelayCommand>,
}

/// A stopped actor and a manager that resolves its live replacement. Chat
/// tests use this hand-written actor instead of mocking the session manager
/// protocol.
#[cfg(test)]
fn replacement_session_test_fixture(
    session_id: &str,
    accepted_ordinal: u64,
) -> ReplacementSessionTestFixture {
    let (stopped_commands, stopped_commands_rx) = mpsc::channel(1);
    drop(stopped_commands_rx);
    let (stopped_releases, stopped_releases_rx) = mpsc::unbounded_channel();
    drop(stopped_releases_rx);
    let (stopped_view_tx, stopped_view) = watch::channel(ManagedSessionView::default());
    drop(stopped_view_tx);
    let stopped = ManagedSessionHandle {
        session_id: session_id.to_owned(),
        commands: stopped_commands,
        releases: stopped_releases,
        view: stopped_view,
    };

    let (commands, mut commands_rx) = mpsc::channel(4);
    let (releases, _releases_rx) = mpsc::unbounded_channel();
    let (view_tx, view) = watch::channel(ManagedSessionView::default());
    let replacement = ManagedSessionHandle {
        session_id: session_id.to_owned(),
        commands,
        releases,
        view,
    };
    let actor_session_id = session_id.to_owned();
    let (submitted_tx, submitted) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _view_tx = view_tx;
        while let Some(command) = commands_rx.recv().await {
            match command {
                ActorCommand::Submit { command, reply, .. } => {
                    // Tests can drop the optional observer when they only
                    // care about acceptance/reconnection.
                    let _ = submitted_tx.send(command);
                    let _ = reply.send(Ok(accepted_ordinal));
                }
                ActorCommand::Sync { reply } => {
                    let _ = reply.send(Ok(()));
                }
                command => command.reject(&actor_session_id, "unsupported test operation"),
            }
        }
    });

    let (manager_commands, mut manager_commands_rx) = mpsc::channel(4);
    let manager_replacement = replacement.clone();
    tokio::spawn(async move {
        while let Some(ManagerCommand::Session {
            session_id: requested,
            reply,
        }) = manager_commands_rx.recv().await
        {
            let resolved =
                (requested == manager_replacement.session_id).then(|| manager_replacement.clone());
            let _ = reply.send(resolved);
        }
    });
    ReplacementSessionTestFixture {
        stopped,
        submitted,
        control: SessionManagerControl {
            commands: manager_commands,
        },
    }
}

#[cfg(test)]
mod tests;
