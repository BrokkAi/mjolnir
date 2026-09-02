//! Multiplexed controller-side ownership of durable ACP relay sessions.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::{mpsc, oneshot, watch};

use crate::hel_archive::verify_archive_streaming;
use crate::hel_credentials::{CredentialSyncSignal, relay_event_credential_sync_reason};
use crate::hel_database::{
    ProjectionApplyOutcome, ProjectionIntegrityError, apply_projection_page,
    save_materialized_session,
};
use crate::hel_elicitation::ElicitationResponse;
use crate::hel_projection::{
    ProjectionIndex, apply_committed_projection_event_indexed, materialized_session_from_canonical,
    project_relay_event_indexed,
};
use crate::hel_state::{ManagedSessionSnapshot, MaterializedSession};
use crate::hel_targets::{
    CancellableProcessExecutor, CommandExecutor, CommandPlan, CommandSpec, TargetRecoveryOutcome,
    TargetRecoveryPlan, ensure_recovery_target_running,
};
use crate::hel_worker::{RelayCommand, RelayCursor, RelayOperationalState};
use crate::hel_worker_client::{
    RelayAttachment, RelayClient, RelayEventPage, RelayRejected, RelayTransportDead,
    StartedReviewer,
};
use crate::hel_worker_runtime::ReviewerLaunchConfig;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRecoveryPlan {
    pub target: Option<TargetRecoveryPlan>,
    pub liveness_probe: CommandSpec,
    /// Refresh a stale installed worker before restarting it. The digest is
    /// computed inside the recovery task so hashing a large binary never
    /// blocks a controller UI loop.
    pub binary_refresh: Option<WorkerBinaryRefreshPlan>,
    /// Keep the worker executable and its launch schema paired. Configuration
    /// bytes travel through redacted stdin only when their digest is stale.
    pub launch_refresh: Option<WorkerLaunchRefreshPlan>,
    pub restart: CommandPlan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBinaryRefreshPlan {
    pub source: PathBuf,
    pub installed_digest: CommandSpec,
    pub replace: CommandPlan,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerRecoveryOutcome {
    Alive,
    Starting,
    TargetMissing,
    RestartedDead,
    RestartedUnresponsive,
}

fn refresh_worker_binary_if_stale(
    executor: &impl CommandExecutor,
    plan: Option<&WorkerBinaryRefreshPlan>,
) -> Result<()> {
    let Some(plan) = plan else {
        return Ok(());
    };
    let expected = crate::hel_worker_runtime::worker_executable_digest(&plan.source)?;
    if installed_digest_matches(executor, &plan.installed_digest, &expected) {
        return Ok(());
    }
    plan.replace
        .execute(executor)
        .context("replace stale relay worker binary")?;
    Ok(())
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

async fn recover_worker(
    plan: WorkerRecoveryPlan,
    restart_unresponsive: bool,
) -> Result<WorkerRecoveryOutcome> {
    tokio::task::spawn_blocking(move || {
        let executor = CancellableProcessExecutor::with_timeout(WORKER_RESTART_TIMEOUT);
        if ensure_recovery_target_running(&executor, plan.target.as_ref())
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
                refresh_worker_binary_if_stale(&executor, plan.binary_refresh.as_ref())?;
                refresh_worker_launch_if_stale(&executor, plan.launch_refresh.as_ref())?;
                plan.restart.execute(&executor)?;
                Ok(WorkerRecoveryOutcome::RestartedUnresponsive)
            }
            "dead" => {
                refresh_worker_binary_if_stale(&executor, plan.binary_refresh.as_ref())?;
                refresh_worker_launch_if_stale(&executor, plan.launch_refresh.as_ref())?;
                plan.restart.execute(&executor)?;
                Ok(WorkerRecoveryOutcome::RestartedDead)
            }
            output => bail!("worker liveness probe returned unexpected output {output:?}"),
        }
    })
    .await
    .context("worker recovery task failed")?
}

/// Why a managed session stopped producing fresh views. The kind matters to
/// callers: an unreachable relay is worth retrying and diagnosing, while a
/// projection integrity failure is deterministic and needs a different report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum ViewError {
    Unreachable(String),
    TargetMissing(String),
    ProjectionIntegrity(String),
}

impl ViewError {
    pub fn detail(&self) -> &str {
        match self {
            Self::Unreachable(detail)
            | Self::TargetMissing(detail)
            | Self::ProjectionIntegrity(detail) => detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ManagedSessionView {
    pub snapshot: Option<ManagedSessionSnapshot>,
    pub connected: bool,
    pub error: Option<ViewError>,
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

/// What a caller asks of a session's second-opinion reviewer.
///
/// The reviewer is a sidecar of the session's worker, so every action travels
/// the session's own relay connection rather than opening a second one.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerAction {
    Start {
        config: Box<ReviewerLaunchConfig>,
    },
    Submit {
        command_id: String,
        command: RelayCommand,
    },
    Attach {
        after_ordinal: u64,
        after_digest: String,
    },
    Acknowledge {
        through_ordinal: u64,
        through_digest: String,
    },
    Status,
    /// Answer a form the reviewer's harness is waiting on. A reviewer left
    /// waiting on one stalls the whole review.
    RespondElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
    },
    Pause,
    /// Report what the workspace repositories changed since these baselines.
    CaptureDelta {
        baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
    /// Record the trees a completed review reviewed through.
    AdvanceBaseline {
        trees: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
    /// Run Bifrost's semantic diff analysis over the captured trees.
    AnalyzeDelta {
        repositories: Vec<crate::hel_worker::AnalyzeDeltaRepository>,
    },
    /// Collect the specialist lanes the review supervisor asked for.
    TakeLaneDispatches,
}

impl ReviewerAction {
    pub const fn operation_name(&self) -> &'static str {
        match self {
            Self::Start { .. } => "reviewer_start",
            Self::Submit { .. } => "reviewer_submit",
            Self::Attach { .. } => "reviewer_attach",
            Self::Acknowledge { .. } => "reviewer_acknowledge",
            Self::Status => "reviewer_status",
            Self::RespondElicitation { .. } => "reviewer_respond_elicitation",
            Self::Pause => "reviewer_pause",
            Self::CaptureDelta { .. } => "reviewer_capture_delta",
            Self::AdvanceBaseline { .. } => "reviewer_advance_baseline",
            Self::AnalyzeDelta { .. } => "reviewer_analyze_delta",
            Self::TakeLaneDispatches => "reviewer_take_lane_dispatches",
        }
    }
}

/// What a [`ReviewerAction`] produced.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewerOutcome {
    Started(Box<StartedReviewer>),
    Accepted {
        ordinal: u64,
    },
    Attached(Box<RelayAttachment>),
    Acknowledged(RelayCursor),
    Status(Box<RelayOperationalState>),
    ElicitationResolved,
    Paused,
    /// What every workspace repository changed since the stored baselines.
    Delta {
        repositories: Vec<crate::hel_worker::RepoDelta>,
    },
    BaselineAdvanced,
    /// Bifrost's changed-callable packet for the captured trees.
    ChangedFunctions {
        packet: String,
    },
    /// Specialist lanes the review supervisor asked for.
    LaneDispatches {
        requests: Vec<crate::hel_review::lanes::ReviewSubagentRequest>,
    },
}

pub enum RemoteSessionRequest {
    Submit {
        session_id: String,
        command_id: String,
        command: RelayCommand,
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
    latest: std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
}

impl SessionRequestOrder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `forward` for `request` after everything already queued for the
    /// same session has finished.
    pub fn dispatch<F, Fut>(&mut self, request: RemoteSessionRequest, forward: F)
    where
        F: FnOnce(RemoteSessionRequest) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        // Sessions that have gone quiet leave a finished handle behind; drop
        // them here so the map tracks live work rather than every session the
        // bridge has ever served.
        self.latest.retain(|_, handle| !handle.is_finished());
        let session_id = request.session_id().to_owned();
        let previous = self.latest.remove(&session_id);
        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                // A panicked predecessor still releases its successor: the
                // request behind it is the user's, and dropping it silently
                // would be worse than running it late.
                let _ = previous.await;
            }
            forward(request).await;
        });
        self.latest.insert(session_id, handle);
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
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn view(&self) -> ManagedSessionView {
        self.view.borrow().clone()
    }

    /// Whether the per-session actor behind this handle has retired. The
    /// manager itself may still be alive with a replacement actor, so callers
    /// holding long-lived handles use this to reacquire the current one.
    pub(crate) fn is_stopped(&self) -> bool {
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

    pub(crate) async fn enqueue_submit(
        &self,
        command_id: String,
        command: RelayCommand,
    ) -> Result<PendingRelaySubmit> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(ActorCommand::Submit {
                command_id,
                command,
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

    pub(crate) async fn enqueue_sync(&self) -> Result<PendingRelaySync> {
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

pub(crate) struct PendingRelaySubmit {
    response: oneshot::Receiver<std::result::Result<u64, String>>,
}

impl PendingRelaySubmit {
    pub(crate) async fn wait(self) -> Result<u64> {
        self.response
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }
}

pub(crate) struct PendingRelaySync {
    response: oneshot::Receiver<std::result::Result<(), String>>,
}

impl PendingRelaySync {
    pub(crate) async fn wait(self) -> Result<()> {
        self.response
            .await
            .context("session manager stopped")?
            .map_err(anyhow::Error::msg)
    }
}

impl SessionManagerControl {
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
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.session(session_id.to_owned()).await {
                Ok(handle) => return Ok(handle),
                Err(error) if tokio::time::Instant::now() < deadline => {
                    tracing::trace!(session_id, "waiting for session actor: {error:#}");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(error),
            }
        }
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
                reply,
            } => RemoteSessionRequest::Submit {
                session_id: session_id.clone(),
                command_id,
                command,
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
                | RemoteSessionRequest::RespondElicitation { reply, .. } => {
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
    let mut interval = tokio::time::interval(SESSION_SYNC_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        lifecycle.set_retirement_requested(*retirement.borrow_and_update());
        if lifecycle.should_stop() {
            break;
        }
        tokio::select! {
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
                            match recover_worker(plan, restart_unresponsive).await {
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
                                        | WorkerRecoveryOutcome::TargetMissing => unreachable!(),
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
                    ActorCommand::Submit { command_id, command, reply } => {
                        // A turn under review holds its session's prompts, and
                        // this is where that is enforced: the process that owns
                        // the review owns the refusal, so no surface can bypass
                        // it and no stale record can outlive it. The review's
                        // own corrective prompt is submitted after the review
                        // resolves, so it is never the one refused.
                        if matches!(command, RelayCommand::Prompt { .. })
                            && let Some(refusal) =
                                crate::hel_review::host::prompt_refusal(&target.session_id)
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
                                reply,
                            });
                            continue;
                        }
                        deliver_submit(
                            &target,
                            &mut connection,
                            command_id,
                            command,
                            reply,
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
                        if lifecycle.is_leased() {
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
                        let operation = action.operation_name();
                        let result = async {
                            sync_actor_connection(&target, &mut connection).await?;
                            let connection =
                                connection.as_mut().context("relay is disconnected")?;
                            drive_reviewer(connection, role, action).await
                        }
                        .await;
                        match &result {
                            Ok(_) => {}
                            Err(error) if !is_final_rejection(error) => connection = None,
                            Err(_) => {}
                        }
                        if let Err(error) = &result {
                            tracing::warn!(
                                session_id = %target.session_id,
                                %operation,
                                error = %error,
                                "reviewer action failed"
                            );
                        }
                        if reply
                            .send(result.map_err(|error| format!("{error:#}")))
                            .is_err()
                        {
                            tracing::debug!(
                                session_id = %target.session_id,
                                %operation,
                                "reviewer result receiver was already closed"
                            );
                        }
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
                            deferred.command_id,
                            deferred.command,
                            deferred.reply,
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
    command_id: String,
    command: RelayCommand,
    reply: oneshot::Sender<std::result::Result<u64, String>>,
    view_tx: &watch::Sender<ManagedSessionView>,
    updates: &CoalescedUpdateSender,
) {
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
async fn drive_reviewer(
    connection: &mut StandaloneSession,
    role: Option<String>,
    action: ReviewerAction,
) -> Result<ReviewerOutcome> {
    let client = &mut connection.client;
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
        *connection = Some(StandaloneSession::connect(target).await?);
        return Ok(Some(
            connection
                .as_ref()
                .expect("connection was initialized")
                .snapshot(),
        ));
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
        let loaded = crate::hel_database::load_materialized_session(&session_id)?;
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
        let changed = repaired
            || self.materialized.applied_event_ordinal != original_ordinal
            || self.materialized.applied_event_digest != original_digest
            || self.operational != original_operational;
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
        let state = crate::hel_database::load_state()?;
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
            window: crate::hel_state::ProjectionWindow::of(&self.materialized),
            materialized: self.materialized.clone(),
            operational: self.operational.clone(),
            latest_credential_sync_signal: self.latest_credential_sync_signal.clone(),
            worker_build: self.client.worker_build().map(str::to_owned),
        }
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

    /// Run a prompt in a disposable ACP session. The result is not session
    /// history, so nothing is projected and no sync is needed.
    pub async fn compact(&mut self, prompt: String) -> Result<String> {
        self.client.compact(prompt).await
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
                        rejected.0.code == crate::hel_worker::RelayErrorCode::InvalidState
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
            let reconciliation = crate::hel_project_memory::reconcile_into_canonical(
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

impl crate::hel_compaction::CompactionBackend for StandaloneSession {
    fn compact<'a>(
        &'a mut self,
        prompt: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(Self::compact(self, prompt))
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

pub fn new_command_id(prefix: &str) -> Result<String> {
    ensure!(!prefix.trim().is_empty(), "command ID prefix is required");
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate command ID: {error}"))?;
    Ok(format!("{prefix}-{}", hex(&random)))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
pub(crate) struct ReplacementSessionTestFixture {
    pub(crate) stopped: ManagedSessionHandle,
    pub(crate) control: SessionManagerControl,
}

/// A stopped actor and a manager that resolves its live replacement. Chat
/// tests use this hand-written actor instead of mocking the session manager
/// protocol.
#[cfg(test)]
pub(crate) fn replacement_session_test_fixture(
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
    tokio::spawn(async move {
        let _view_tx = view_tx;
        while let Some(command) = commands_rx.recv().await {
            match command {
                ActorCommand::Submit { reply, .. } => {
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
        control: SessionManagerControl {
            commands: manager_commands,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
    use sha2::Digest;

    fn ordering_request(session_id: &str, command_id: &str) -> RemoteSessionRequest {
        let (reply, _response) = oneshot::channel();
        RemoteSessionRequest::Submit {
            session_id: session_id.into(),
            command_id: command_id.into(),
            command: RelayCommand::SetConfig {
                key: "effort".into(),
                value: "high".into(),
            },
            reply,
        }
    }

    /// `/effort` followed by a prompt has to reach the relay that way round,
    /// or the prompt runs under the old setting. A bridge that spawns every
    /// request concurrently loses that, so the order is pinned here: the
    /// first request is held up, and the second must not overtake it.
    #[tokio::test]
    async fn one_session_keeps_its_requests_in_the_order_they_were_made() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Notify::new());
        let mut order = SessionRequestOrder::new();

        for command_id in ["first", "second", "third"] {
            let observed = Arc::clone(&observed);
            let release = Arc::clone(&release);
            order.dispatch(ordering_request("session-a", command_id), move |request| {
                let RemoteSessionRequest::Submit { command_id, .. } = request else {
                    unreachable!("the fixture only submits")
                };
                async move {
                    // Only the first request waits. If the order were lost,
                    // the other two would finish while it is held.
                    if command_id == "first" {
                        release.notified().await;
                    }
                    observed.lock().unwrap().push(command_id);
                }
            });
        }

        // Nothing may run while the first request is held. Yield generously:
        // the point is that the later requests never get to run, not that
        // they have not been polled yet.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert!(
            observed.lock().unwrap().is_empty(),
            "a later request overtook the one being held: {:?}",
            observed.lock().unwrap()
        );

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while observed.lock().unwrap().len() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("every request ran");
        assert_eq!(*observed.lock().unwrap(), ["first", "second", "third"]);
    }

    /// Ordering is per session: one session waiting on a slow relay must not
    /// hold up another session's prompt.
    #[tokio::test]
    async fn different_sessions_still_overlap() {
        let finished = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(tokio::sync::Notify::new());
        let mut order = SessionRequestOrder::new();

        let held = Arc::clone(&release);
        let recorder = Arc::clone(&finished);
        order.dispatch(ordering_request("session-a", "slow"), move |_| async move {
            held.notified().await;
            recorder.lock().unwrap().push("slow");
        });
        let recorder = Arc::clone(&finished);
        order.dispatch(ordering_request("session-b", "fast"), move |_| async move {
            recorder.lock().unwrap().push("fast");
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while finished.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the other session ran while the first was held");
        assert_eq!(*finished.lock().unwrap(), ["fast"]);

        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while finished.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the held request ran once released");
    }

    /// A session that has gone quiet must not leave a handle behind for ever:
    /// a long-lived daemon serves many sessions.
    #[tokio::test]
    async fn finished_sessions_are_forgotten() {
        let mut order = SessionRequestOrder::new();
        for index in 0..8 {
            order.dispatch(
                ordering_request(&format!("session-{index}"), "only"),
                |_| async {},
            );
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while order.latest.values().any(|handle| !handle.is_finished()) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the request finished");
        }
        // The next dispatch prunes what has finished, so the map tracks live
        // work rather than every session ever seen.
        order.dispatch(ordering_request("session-last", "only"), |_| async {});
        assert_eq!(order.latest.len(), 1);
    }

    /// A reviewer action reaches a remote controller daemon as JSON, so both
    /// halves of the exchange have to survive that round trip intact.
    #[test]
    fn reviewer_actions_and_outcomes_survive_the_daemon_wire() {
        let config = ReviewerLaunchConfig {
            profile_id: "claude".into(),
            harness: crate::hel_config::HarnessKind::Claude,
            bridge_command: "npx".into(),
            bridge_args: vec!["claude-code-acp".into()],
            environment: BTreeMap::from([("EXTRA".into(), "1".into())]),
            execution_policy: crate::hel_config::ExecutionPolicy::Unconstrained,
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            generation: 2,
            mcp_servers: Vec::new(),
        };
        let actions = [
            ReviewerAction::Start {
                config: Box::new(config),
            },
            ReviewerAction::Submit {
                command_id: "review-1".into(),
                command: RelayCommand::Cancel,
            },
            ReviewerAction::Attach {
                after_ordinal: 4,
                after_digest: "digest".into(),
            },
            ReviewerAction::Acknowledge {
                through_ordinal: 4,
                through_digest: "digest".into(),
            },
            ReviewerAction::Status,
            ReviewerAction::Pause,
            ReviewerAction::CaptureDelta {
                baselines: BTreeMap::from([(std::path::PathBuf::from("/w/app"), "tree".into())]),
            },
            ReviewerAction::AdvanceBaseline {
                trees: BTreeMap::from([(std::path::PathBuf::from("/w/app"), "tree".into())]),
            },
            ReviewerAction::AnalyzeDelta {
                repositories: vec![crate::hel_worker::AnalyzeDeltaRepository {
                    root: std::path::PathBuf::from("/w/app"),
                    baseline_tree: Some("base".into()),
                    current_tree: "target".into(),
                }],
            },
        ];
        for action in actions {
            let encoded = serde_json::to_string(&action).unwrap();
            let decoded: ReviewerAction = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, action);
        }

        let outcome = ReviewerOutcome::Accepted { ordinal: 9 };
        let encoded = serde_json::to_string(&outcome).unwrap();
        let decoded: ReviewerOutcome = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(decoded, ReviewerOutcome::Accepted { ordinal: 9 }));

        let paused = serde_json::to_string(&ReviewerOutcome::Paused).unwrap();
        assert!(matches!(
            serde_json::from_str::<ReviewerOutcome>(&paused).unwrap(),
            ReviewerOutcome::Paused
        ));

        let delta = ReviewerOutcome::Delta {
            repositories: vec![crate::hel_worker::RepoDelta {
                root: std::path::PathBuf::from("/w/app"),
                baseline_tree: None,
                current_tree: "target".into(),
                patch: "diff --git a/a b/a\n".into(),
                diffstat: "1 file changed".into(),
                changed_lines: 1,
            }],
        };
        let encoded = serde_json::to_string(&delta).unwrap();
        let ReviewerOutcome::Delta { repositories } =
            serde_json::from_str::<ReviewerOutcome>(&encoded).unwrap()
        else {
            panic!("a captured delta must survive the daemon wire");
        };
        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0].current_tree, "target");
    }

    /// Every reviewer action names itself for the actor's logs and for the
    /// rejection path, so a stalled review can be traced to the step it stalled
    /// on.
    #[test]
    fn every_reviewer_action_names_its_operation() {
        let names = [
            ReviewerAction::Submit {
                command_id: String::new(),
                command: RelayCommand::Cancel,
            }
            .operation_name(),
            ReviewerAction::Attach {
                after_ordinal: 0,
                after_digest: String::new(),
            }
            .operation_name(),
            ReviewerAction::Acknowledge {
                through_ordinal: 0,
                through_digest: String::new(),
            }
            .operation_name(),
            ReviewerAction::Status.operation_name(),
            ReviewerAction::Pause.operation_name(),
        ];
        assert_eq!(
            names,
            [
                "reviewer_submit",
                "reviewer_attach",
                "reviewer_acknowledge",
                "reviewer_status",
                "reviewer_pause",
            ]
        );
        assert!(names.iter().all(|name| name.starts_with("reviewer_")));
    }

    #[test]
    fn reconnect_delay_backs_off_and_stops_at_the_ceiling() {
        assert_eq!(reconnect_delay(1), RECONNECT_INTERVAL);
        assert_eq!(reconnect_delay(2), Duration::from_secs(2));
        assert_eq!(reconnect_delay(4), Duration::from_secs(8));
        assert_eq!(reconnect_delay(6), RECONNECT_BACKOFF_CEILING);
        assert_eq!(reconnect_delay(u32::MAX), RECONNECT_BACKOFF_CEILING);
    }

    #[test]
    fn only_dead_worker_connection_failures_request_a_restart() {
        // The wording is deliberately unlike anything a matcher could have
        // been written against: the marker, not the message, decides.
        let reworded = anyhow::Error::new(RelayTransportDead::new(
            "the session proxy vanished mid-conversation",
        ))
        .context("connect to the session worker for checkpoint");
        assert!(worker_connect_needs_restart(&reworded), "{reworded:#}");
        assert!(!worker_connect_allows_live_restart(&reworded));

        // Text alone proves nothing now, not even the exact text the producing
        // sites still use: an unmarked failure must never restart a worker.
        for detail in [
            "relay proxy disconnected during hello",
            "Connection refused (os error 111)",
            "relay negotiated unsupported protocol 9",
            "controller projection is corrupt",
        ] {
            assert!(!worker_connect_needs_restart(&anyhow::anyhow!(detail)));
        }
    }

    /// The producing side of the same contract: a proxy that dies without
    /// serving the handshake must ask for a worker restart, whatever its
    /// failure happens to read like.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_proxy_that_dies_before_hello_requests_a_worker_restart() {
        let mut dead = target("sh");
        dead.spec = CommandSpec::new("sh", ["-c", "exit 1"]).purpose("dead relay proxy fixture");

        let error = StandaloneSession::connect(&dead)
            .await
            .err()
            .expect("a proxy that exits cannot serve a session");

        assert!(worker_connect_needs_restart(&error), "{error:#}");
        assert!(worker_connect_allows_live_restart(&error));
    }

    /// A lease answer crosses a channel. Formatting the failure into a string
    /// there would strip the cause and silently cost the checkpoint path its
    /// restart decision, so prove the typed cause survives the handoff.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_lease_keeps_the_cause_that_decides_a_worker_restart() {
        let (commands_tx, commands_rx) = mpsc::channel(4);
        let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
        let (_retirement_tx, retirement_rx) = watch::channel(false);
        let (view_tx, _view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, _updates_rx) = coalesced_update_channel();
        let mut dead = target("sh");
        dead.spec = CommandSpec::new("sh", ["-c", "exit 1"]).purpose("dead relay proxy fixture");
        tokio::spawn(run_session_actor(
            dead,
            commands_rx,
            releases_rx,
            retirement_rx,
            view_tx,
            updates_tx,
        ));

        let (reply, response) = oneshot::channel();
        commands_tx
            .send(ActorCommand::Lease { reply })
            .await
            .unwrap();
        let error = response
            .await
            .expect("actor answered the lease request")
            .err()
            .expect("a dead proxy cannot be leased");

        assert!(worker_connect_needs_restart(&error), "{error:#}");
    }

    #[tokio::test]
    async fn recovery_restarts_a_live_worker_only_after_a_failed_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let restarted = directory.path().join("restarted");
        let recovery = |liveness: &str| WorkerRecoveryPlan {
            target: None,
            liveness_probe: CommandSpec::new("printf", [format!("{liveness}\n")])
                .purpose("probe test worker liveness"),
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "restart test worker".into(),
                commands: vec![
                    CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                        .purpose("restart test worker"),
                ],
            },
        };

        assert_eq!(
            recover_worker(recovery("alive"), false).await.unwrap(),
            WorkerRecoveryOutcome::Alive
        );
        assert!(!restarted.exists(), "a live worker must not be restarted");

        assert_eq!(
            recover_worker(recovery("starting"), true).await.unwrap(),
            WorkerRecoveryOutcome::Starting
        );
        assert!(
            !restarted.exists(),
            "a worker recovering its journal must not be restarted"
        );

        assert_eq!(
            recover_worker(recovery("alive"), true).await.unwrap(),
            WorkerRecoveryOutcome::RestartedUnresponsive
        );
        assert!(
            restarted.exists(),
            "a worker that cannot serve a fresh handshake is restarted"
        );
        std::fs::remove_file(&restarted).unwrap();

        assert_eq!(
            recover_worker(recovery("dead"), false).await.unwrap(),
            WorkerRecoveryOutcome::RestartedDead
        );
        assert!(restarted.exists(), "a confirmed dead worker is restarted");
    }

    #[tokio::test]
    async fn recovery_replaces_only_a_stale_worker_binary_before_restart() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("current-worker");
        let refreshed = directory.path().join("worker-refreshed");
        let restarted = directory.path().join("worker-restarted");
        std::fs::write(&source, b"current worker binary").unwrap();
        let current_digest = format!("{:x}", sha2::Sha256::digest(b"current worker binary"));
        let recovery = |installed_digest: &str, require_refresh: bool| {
            let mut restart = if require_refresh {
                CommandSpec::new(
                    "sh",
                    [
                        "-c",
                        "test -f \"$MJ_TEST_REFRESHED\" && touch -- \"$MJ_TEST_RESTARTED\"",
                    ],
                )
            } else {
                CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
            }
            .purpose("restart test worker");
            restart.env.insert(
                "MJ_TEST_REFRESHED".into(),
                refreshed.to_string_lossy().into_owned(),
            );
            restart.env.insert(
                "MJ_TEST_RESTARTED".into(),
                restarted.to_string_lossy().into_owned(),
            );
            WorkerRecoveryPlan {
                target: None,
                liveness_probe: CommandSpec::new("printf", ["dead\n"])
                    .purpose("probe test worker liveness"),
                binary_refresh: Some(WorkerBinaryRefreshPlan {
                    source: source.clone(),
                    installed_digest: CommandSpec::new(
                        "printf",
                        [format!("{installed_digest}  /worker/hel\n")],
                    )
                    .purpose("identify test worker binary"),
                    replace: CommandPlan {
                        description: "refresh test worker".into(),
                        commands: vec![
                            CommandSpec::new("touch", [refreshed.to_string_lossy().into_owned()])
                                .purpose("refresh test worker"),
                        ],
                    },
                }),
                launch_refresh: None,
                restart: CommandPlan {
                    description: "restart test worker".into(),
                    commands: vec![restart],
                },
            }
        };

        assert_eq!(
            recover_worker(recovery(&current_digest, false), false)
                .await
                .unwrap(),
            WorkerRecoveryOutcome::RestartedDead
        );
        assert!(!refreshed.exists(), "a current binary must not be copied");
        assert!(restarted.exists());

        std::fs::remove_file(&restarted).unwrap();
        assert_eq!(
            recover_worker(recovery(&"0".repeat(64), true), false)
                .await
                .unwrap(),
            WorkerRecoveryOutcome::RestartedDead
        );
        assert!(refreshed.exists(), "a stale binary must be refreshed");
        assert!(restarted.exists(), "refresh must finish before restart");
    }

    #[tokio::test]
    async fn recovery_refreshes_a_stale_launch_config_before_restart() {
        let directory = tempfile::tempdir().unwrap();
        let refreshed = directory.path().join("launch-refreshed");
        let restarted = directory.path().join("worker-restarted");
        let mut restart = CommandSpec::new(
            "sh",
            [
                "-c",
                "test -f \"$MJ_TEST_REFRESHED\" && touch -- \"$MJ_TEST_RESTARTED\"",
            ],
        )
        .purpose("restart test worker");
        restart.env.insert(
            "MJ_TEST_REFRESHED".into(),
            refreshed.to_string_lossy().into_owned(),
        );
        restart.env.insert(
            "MJ_TEST_RESTARTED".into(),
            restarted.to_string_lossy().into_owned(),
        );
        let outcome = recover_worker(
            WorkerRecoveryPlan {
                target: None,
                liveness_probe: CommandSpec::new("printf", ["dead\n"])
                    .purpose("probe test worker liveness"),
                binary_refresh: None,
                launch_refresh: Some(WorkerLaunchRefreshPlan {
                    expected_sha256: "a".repeat(64),
                    installed_digest: CommandSpec::new(
                        "printf",
                        [format!("{}  /worker/launch.json\n", "b".repeat(64))],
                    )
                    .purpose("identify test launch config"),
                    replace: CommandPlan {
                        description: "refresh test launch config".into(),
                        commands: vec![
                            CommandSpec::new("touch", [refreshed.to_string_lossy().into_owned()])
                                .purpose("refresh test launch config"),
                        ],
                    },
                }),
                restart: CommandPlan {
                    description: "restart test worker".into(),
                    commands: vec![restart],
                },
            },
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome, WorkerRecoveryOutcome::RestartedDead);
        assert!(refreshed.exists());
        assert!(
            restarted.exists(),
            "config refresh must finish before restart"
        );
    }

    #[tokio::test]
    async fn recovery_starts_a_stopped_target_before_probing_its_worker() {
        let directory = tempfile::tempdir().unwrap();
        let target_started = directory.path().join("target-started");
        let worker_restarted = directory.path().join("worker-restarted");
        let inspection = |status: &str| {
            serde_json::to_string(&serde_json::json!([{
                "Config": { "Labels": {
                    (crate::hel_targets::MANAGED_LABEL): "true",
                    (crate::hel_targets::SESSION_LABEL): "session-1",
                }},
                "State": { "Status": status },
            }]))
            .unwrap()
        };
        let mut inspect = CommandSpec::new(
            "sh",
            [
                "-c",
                "if [ -f \"$MJ_TEST_TARGET_STARTED\" ]; then printf '%s\\n' \"$MJ_TEST_RUNNING\"; else printf '%s\\n' \"$MJ_TEST_EXITED\"; fi",
            ],
        )
        .purpose("inspect test target");
        inspect.env.insert(
            "MJ_TEST_TARGET_STARTED".into(),
            target_started.to_string_lossy().into_owned(),
        );
        inspect
            .env
            .insert("MJ_TEST_RUNNING".into(), inspection("running"));
        inspect
            .env
            .insert("MJ_TEST_EXITED".into(), inspection("exited"));
        let mut start = CommandSpec::new("sh", ["-c", "touch -- \"$MJ_TEST_TARGET_STARTED\""])
            .purpose("start test target");
        start.env.insert(
            "MJ_TEST_TARGET_STARTED".into(),
            target_started.to_string_lossy().into_owned(),
        );
        let mut liveness = CommandSpec::new(
            "sh",
            [
                "-c",
                "test -f \"$MJ_TEST_TARGET_STARTED\" && printf 'dead\\n'",
            ],
        )
        .purpose("probe test worker after target start");
        liveness.env.insert(
            "MJ_TEST_TARGET_STARTED".into(),
            target_started.to_string_lossy().into_owned(),
        );

        let outcome = recover_worker(
            WorkerRecoveryPlan {
                target: Some(TargetRecoveryPlan {
                    exists: CommandSpec::new("true", std::iter::empty::<&str>())
                        .purpose("check test target"),
                    inspect,
                    start,
                    session_id: "session-1".into(),
                }),
                liveness_probe: liveness,
                binary_refresh: None,
                launch_refresh: None,
                restart: CommandPlan {
                    description: "restart test worker".into(),
                    commands: vec![
                        CommandSpec::new(
                            "touch",
                            [worker_restarted.to_string_lossy().into_owned()],
                        )
                        .purpose("restart test worker"),
                    ],
                },
            },
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome, WorkerRecoveryOutcome::RestartedDead);
        assert!(target_started.exists());
        assert!(worker_restarted.exists());
    }

    #[tokio::test]
    async fn recovery_reports_a_missing_target_without_running_worker_commands() {
        let unreachable = CommandSpec::new("false", std::iter::empty::<&str>());
        let outcome = recover_worker(
            WorkerRecoveryPlan {
                target: Some(TargetRecoveryPlan {
                    exists: unreachable,
                    inspect: CommandSpec::new("false", std::iter::empty::<&str>()),
                    start: CommandSpec::new("false", std::iter::empty::<&str>()),
                    session_id: "session-1".into(),
                }),
                liveness_probe: CommandSpec::new("false", std::iter::empty::<&str>()),
                binary_refresh: None,
                launch_refresh: None,
                restart: CommandPlan {
                    description: "must not restart".into(),
                    commands: vec![CommandSpec::new("false", std::iter::empty::<&str>())],
                },
            },
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, WorkerRecoveryOutcome::TargetMissing);
    }

    fn target(program: &str) -> RelaySessionTarget {
        RelaySessionTarget {
            session_id: "session-1".to_owned(),
            spec: CommandSpec::new(program, std::iter::empty::<&str>()),
            worker_recovery: None,
            project_memory: None,
        }
    }

    /// A connected view carrying a conversation, so republishing it exercises
    /// the case a whole-transcript comparison would have to walk.
    fn view_at_ordinal(ordinal: u64) -> ManagedSessionView {
        let digest = "a".repeat(64);
        let mut materialized = MaterializedSession::empty("session-1");
        materialized.applied_event_ordinal = ordinal;
        materialized.applied_event_digest = digest.clone();
        materialized.transcript = (1..=200)
            .map(|position| {
                Arc::new(crate::hel_state::TranscriptItem {
                    stable_id: format!("system:{position}"),
                    position,
                    latest_content_event_ordinal: None,
                    created_at_ms: 1,
                    last_changed_at_ms: 1,
                    body: crate::hel_state::TranscriptBody::System {
                        text: format!("event {position}"),
                    },
                })
            })
            .collect();
        ManagedSessionView {
            snapshot: Some(ManagedSessionSnapshot {
                window: crate::hel_state::ProjectionWindow::of(&materialized),
                materialized,
                operational: RelayOperationalState {
                    session_id: "session-1".into(),
                    execution: crate::hel_worker::RelayExecutionState::Idle,
                    latest_ordinal: ordinal,
                    latest_digest: digest.clone(),
                    acknowledged_through: ordinal,
                    acknowledged_digest: digest,
                    recovery_floor_ordinal: 0,
                    recovery_floor_digest: crate::hel_worker::RELAY_EVENT_GENESIS_DIGEST.into(),
                    native_session_id: None,
                    agent_capabilities: None,
                    agent_info: None,
                    config_options: Vec::new(),
                    modes: None,
                    available_commands: Vec::new(),
                    config: BTreeMap::new(),
                    active_prompt: None,
                    queued_prompts: Vec::new(),
                    active_user_shells: Vec::new(),
                    active_agent_terminals: Vec::new(),
                    checkpoint_barrier: None,
                    checkpoint_ready: None,
                    last_acp_activity_at_ms: None,
                    harness_turn: None,
                    last_harness_turn_started_ordinal: None,
                    background_commands: Vec::new(),
                },
                latest_credential_sync_signal: None,
                worker_build: None,
            }),
            connected: true,
            error: None,
        }
    }

    #[test]
    fn republishing_an_unchanged_view_notifies_nobody() {
        let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, mut updates_rx) = coalesced_update_channel();

        publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
        assert!(view_rx.has_changed().expect("watch stays open"));
        assert_eq!(
            updates_rx.try_recv().expect("the first view is news").view,
            view_at_ordinal(7)
        );
        let _ = view_rx.borrow_and_update();

        publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);

        assert!(
            !view_rx.has_changed().expect("watch stays open"),
            "a sync tick that moved nothing must not wake the dashboard"
        );
        assert!(updates_rx.try_recv().is_err());
    }

    #[test]
    fn publishing_an_advanced_event_frontier_notifies_watchers() {
        let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, mut updates_rx) = coalesced_update_channel();
        publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
        let _ = updates_rx.try_recv();
        let _ = view_rx.borrow_and_update();

        publish_view("session-1", view_at_ordinal(8), &view_tx, &updates_tx);

        assert!(view_rx.has_changed().expect("watch stays open"));
        let update = updates_rx.try_recv().expect("the advance is news");
        assert_eq!(update.session_id, "session-1");
        assert_eq!(
            update
                .view
                .snapshot
                .expect("published snapshot")
                .materialized
                .applied_event_ordinal,
            8
        );
    }

    #[test]
    fn publishing_relay_state_that_moved_without_the_frontier_notifies_watchers() {
        let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, mut updates_rx) = coalesced_update_channel();
        publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
        let _ = updates_rx.try_recv();
        let _ = view_rx.borrow_and_update();

        let mut view = view_at_ordinal(7);
        view.snapshot
            .as_mut()
            .expect("published snapshot")
            .operational
            .execution = crate::hel_worker::RelayExecutionState::Running;
        publish_view("session-1", view, &view_tx, &updates_tx);

        assert!(view_rx.has_changed().expect("watch stays open"));
        assert!(updates_rx.try_recv().is_ok());
    }

    #[test]
    fn losing_the_relay_republishes_the_same_snapshot_as_disconnected() {
        let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, mut updates_rx) = coalesced_update_channel();
        publish_view("session-1", view_at_ordinal(7), &view_tx, &updates_tx);
        let _ = updates_rx.try_recv();
        let _ = view_rx.borrow_and_update();

        let mut view = view_at_ordinal(7);
        view.connected = false;
        view.error = Some(ViewError::Unreachable("relay is unreachable".into()));
        publish_view("session-1", view, &view_tx, &updates_tx);

        assert!(view_rx.has_changed().expect("watch stays open"));
        assert!(updates_rx.try_recv().is_ok());
    }

    #[test]
    fn command_ids_are_namespaced_and_unique() {
        let first = new_command_id("prompt").unwrap();
        let second = new_command_id("prompt").unwrap();
        assert!(first.starts_with("prompt-"));
        assert_ne!(first, second);
    }

    #[test]
    fn leased_actor_defers_replacement_and_uses_latest_queued_target() {
        let original = target("relay-v1");
        let intermediate = target("relay-v2");
        let latest = target("relay-v3");
        let mut lifecycle = ActorLifecycle::default();
        lifecycle.activate_lease(7);

        assert_eq!(
            reconcile_action(Some(&original), Some(&intermediate)),
            ReconcileAction::Retire
        );
        lifecycle.set_retirement_requested(true);
        assert!(!lifecycle.accepts_new_work());
        assert!(!lifecycle.should_stop());

        assert_eq!(
            reconcile_action(Some(&original), Some(&latest)),
            ReconcileAction::Retire
        );
        assert!(lifecycle.return_lease(7));
        assert!(lifecycle.should_stop());

        assert_eq!(
            reconcile_action(None, Some(&latest)),
            ReconcileAction::Spawn
        );
    }

    #[test]
    fn leased_actor_defers_removal_until_its_connection_returns() {
        let original = target("relay-v1");
        let mut lifecycle = ActorLifecycle::default();
        lifecycle.activate_lease(11);

        assert_eq!(
            reconcile_action(Some(&original), None),
            ReconcileAction::Retire
        );
        lifecycle.set_retirement_requested(true);
        assert!(!lifecycle.should_stop());
        assert!(!lifecycle.return_lease(10));
        assert!(!lifecycle.should_stop());
        assert!(lifecycle.return_lease(11));
        assert!(lifecycle.should_stop());
        assert_eq!(reconcile_action(None, None), ReconcileAction::Idle);
    }

    #[test]
    fn queued_change_back_to_current_target_cancels_retirement() {
        let original = target("relay-v1");
        let replacement = target("relay-v2");
        let mut lifecycle = ActorLifecycle::default();
        lifecycle.activate_lease(3);

        assert_eq!(
            reconcile_action(Some(&original), Some(&replacement)),
            ReconcileAction::Retire
        );
        lifecycle.set_retirement_requested(true);
        assert_eq!(
            reconcile_action(Some(&original), Some(&original)),
            ReconcileAction::Keep
        );
        lifecycle.set_retirement_requested(false);

        assert!(lifecycle.return_lease(3));
        assert!(!lifecycle.should_stop());
        assert!(lifecycle.accepts_new_work());
    }

    #[tokio::test]
    async fn stopped_actor_is_replaced_without_late_completion_removing_replacement() {
        let desired = target("sh");
        let desired_targets = target_map(std::slice::from_ref(&desired));
        let mut actors = BTreeMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        let (commands, commands_rx) = mpsc::channel(1);
        drop(commands_rx);
        let (releases, _releases_rx) = mpsc::unbounded_channel();
        let (retirement, _retirement_rx) = watch::channel(false);
        let (_view_tx, view) = watch::channel(ManagedSessionView::default());
        let old_abort = tasks.spawn(async { "session-1".to_owned() });
        let old_task_id = old_abort.id();
        actors.insert(
            "session-1".to_owned(),
            ActorRegistration {
                target: desired.clone(),
                commands,
                releases,
                retirement,
                view,
                abort: old_abort,
            },
        );
        let (updates, _updates_rx) = coalesced_update_channel();

        reconcile_actors(&desired_targets, &mut actors, &mut tasks, &updates);

        let replacement_task_id = actors["session-1"].abort.id();
        assert_ne!(replacement_task_id, old_task_id);
        assert!(!actors["session-1"].commands.is_closed());
        assert_eq!(remove_actor_task(&mut actors, old_task_id), None);
        assert_eq!(actors["session-1"].abort.id(), replacement_task_id);
        tasks.abort_all();
    }

    const UNREACHABLE_VIEW_TEST_CHILD: &str = "MJ_TEST_UNREACHABLE_RELAY_CHILD";

    #[tokio::test(start_paused = true)]
    async fn unreachable_relay_publishes_error_view() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in
        // an exact child test instead of racing unrelated tests in this
        // process.
        if std::env::var_os(UNREACHABLE_VIEW_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test_name = format!(
                "{}::unreachable_relay_publishes_error_view",
                module_path!()
                    .strip_prefix("hel::")
                    .unwrap_or(module_path!())
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(UNREACHABLE_VIEW_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated unreachable relay test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // A regression in the publish path deadlocks the actor instead of
        // returning an error, so convert a hang into a hard failure.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(60));
            eprintln!("unreachable relay error view was never published");
            std::process::exit(101);
        });

        let (_commands_tx, commands_rx) = mpsc::channel(4);
        let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
        let (_retirement_tx, retirement_rx) = watch::channel(false);
        let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, mut updates_rx) = coalesced_update_channel();
        tokio::spawn(run_session_actor(
            target("hel-relay-program-that-does-not-exist"),
            commands_rx,
            releases_rx,
            retirement_rx,
            view_tx,
            updates_tx,
        ));

        loop {
            view_rx.changed().await.unwrap();
            let view = view_rx.borrow_and_update().clone();
            if !view.connected {
                let error = view
                    .error
                    .expect("unreachable view carries the connect error");
                assert!(
                    error.detail().contains("session relay proxy"),
                    "unexpected error: {error:?}"
                );
                break;
            }
        }
        let update = updates_rx
            .recv()
            .await
            .expect("dashboard feed received the error view");
        assert_eq!(update.session_id, "session-1");
        assert!(!update.view.connected);
    }

    const UNREADABLE_PROJECTION_TEST_CHILD: &str = "MJ_TEST_UNREADABLE_PROJECTION_CHILD";

    #[tokio::test]
    async fn connecting_to_an_absent_worker_never_reads_the_projection() {
        // MJ_DATA_DIR is process-global, so run the database-backed half in
        // an exact child test instead of racing unrelated tests in this
        // process.
        if std::env::var_os(UNREADABLE_PROJECTION_TEST_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            // A directory where the database file belongs makes every
            // projection read fail, so a read that happens at all shows up in
            // the reported error.
            std::fs::create_dir(directory.path().join("mj.sqlite3")).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!(
                        "{}::connecting_to_an_absent_worker_never_reads_the_projection",
                        module_path!()
                            .strip_prefix("hel::")
                            .unwrap_or(module_path!())
                    ),
                    "--nocapture",
                ])
                .env(UNREADABLE_PROJECTION_TEST_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated projection ordering test failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        assert!(
            crate::hel_database::load_materialized_session("session-1").is_err(),
            "this store must fail every projection read for the test to mean anything"
        );
        let connected =
            StandaloneSession::connect(&target("hel-relay-program-that-does-not-exist")).await;
        let error = match connected {
            Ok(_) => panic!("a relay program that does not exist cannot connect"),
            Err(error) => error,
        };
        let detail = format!("{error:#}");
        assert!(
            detail.contains("session relay proxy"),
            "unexpected error: {detail}"
        );
        assert!(
            !detail.contains("Mjolnir database"),
            "connect read the projection before it reached the relay: {detail}"
        );
    }

    const LEASED_RELAY_ROOT: &str = "MJ_TEST_LEASED_RELAY_ROOT";
    #[cfg(unix)]
    const AUTO_RESTART_TEST_CHILD: &str = "MJ_TEST_AUTO_RESTART_CHILD";
    #[cfg(unix)]
    const AUTO_RESTART_MARKER: &str = "MJ_TEST_AUTO_RESTART_MARKER";
    #[cfg(unix)]
    const DEFERRED_SUBMIT_TEST_CHILD: &str = "MJ_TEST_DEFERRED_SUBMIT_CHILD";
    #[cfg(unix)]
    const RETIRED_SUBMIT_TEST_CHILD: &str = "MJ_TEST_RETIRED_SUBMIT_CHILD";
    #[cfg(unix)]
    const RETURNED_LEASE_VIEW_TEST_CHILD: &str = "MJ_TEST_RETURNED_LEASE_VIEW_CHILD";
    #[cfg(unix)]
    const EXPLICIT_MEMORY_SYNC_TEST_CHILD: &str = "MJ_TEST_EXPLICIT_MEMORY_SYNC_CHILD";
    #[cfg(unix)]
    const SUBMIT_WITHOUT_SYNC_TEST_CHILD: &str = "MJ_TEST_SUBMIT_WITHOUT_SYNC_CHILD";
    #[cfg(unix)]
    const MANAGER_SHUTDOWN_TEST_CHILD: &str = "MJ_TEST_MANAGER_SHUTDOWN_CHILD";
    const LEASED_RELAY_SESSION: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

    /// Relay server half of the leased-submission tests. It does nothing unless
    /// a parent test points it at a relay journal root.
    #[test]
    fn leased_relay_child_serves_stdio() {
        let Some(root) = std::env::var_os(LEASED_RELAY_ROOT) else {
            return;
        };
        // With `--nocapture` libtest writes `test <name> ... ` without a
        // trailing newline before the body runs. End that line first so it
        // cannot glue itself onto the first protocol frame.
        println!();
        let mut relay = crate::hel_worker::DurableRelay::open(
            std::path::Path::new(&root),
            LEASED_RELAY_SESSION,
            "1.0.0",
        )
        .expect("open the test relay journal");
        crate::hel_worker::serve_relay_json_lines(
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
            &mut relay,
        )
        .expect("serve relay frames until the controller disconnects");
    }

    #[cfg(unix)]
    fn exact_test_name(test: &str) -> String {
        format!(
            "{}::{test}",
            module_path!()
                .strip_prefix("hel::")
                .unwrap_or(module_path!())
        )
    }

    /// MJ_DATA_DIR is process-global, so every test that reaches the
    /// controller database runs in an exact child with its own data directory.
    #[cfg(unix)]
    fn run_in_isolated_child(marker: &str, test: &str) {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &exact_test_name(test), "--nocapture"])
            .env(marker, "1")
            .env("MJ_DATA_DIR", directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated {test} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_manager_shutdown_joins_a_live_relay_actor() {
        if std::env::var_os(MANAGER_SHUTDOWN_TEST_CHILD).is_none() {
            run_in_isolated_child(
                MANAGER_SHUTDOWN_TEST_CHILD,
                "session_manager_shutdown_joins_a_live_relay_actor",
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::hel_database::install_isolated_test_writer();
        register_leased_relay_session();
        let relay_root = tempfile::tempdir().unwrap();
        let SessionManagerChannels {
            targets,
            control,
            updates: _updates,
            shutdown,
        } = spawn_session_manager().expect("spawn the session manager");
        targets.send_replace(vec![leased_relay_target(relay_root.path())]);
        let session = control
            .wait_for_session(LEASED_RELAY_SESSION, Duration::from_secs(2))
            .await
            .expect("manager registered the relay actor");
        session
            .sync_now()
            .await
            .expect("relay actor established a live connection");
        assert!(session.view().connected);

        tokio::time::timeout(Duration::from_secs(2), shutdown.shutdown())
            .await
            .expect("manager shutdown stayed within its deadline")
            .expect("manager shutdown task completed cleanly");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relay_attach_does_not_probe_or_install_project_memory() {
        if std::env::var_os(EXPLICIT_MEMORY_SYNC_TEST_CHILD).is_none() {
            run_in_isolated_child(
                EXPLICIT_MEMORY_SYNC_TEST_CHILD,
                "relay_attach_does_not_probe_or_install_project_memory",
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::hel_database::install_isolated_test_writer();
        register_leased_relay_session();
        let relay_root = tempfile::tempdir().unwrap();
        let canonical = tempfile::tempdir().unwrap();
        let mut target = leased_relay_target(relay_root.path());
        target.project_memory = Some(ProjectMemorySyncTarget {
            canonical_root: canonical.path().to_path_buf(),
        });

        let mut connection = StandaloneSession::connect(&target)
            .await
            .expect("relay attach must not depend on its memory endpoint");
        assert!(
            connection.project_memory.is_some(),
            "attach must leave memory pending for an explicit checkpoint sync"
        );

        connection
            .sync_project_memory()
            .await
            .expect("an explicit sync may detect a legacy memory endpoint");
        assert!(
            connection.project_memory.is_none(),
            "the explicit sync reached the relay and disabled its unavailable endpoint"
        );
    }

    /// Catching the local projection up to an accepted command is the
    /// expensive half of a submit, and a caller waiting to hear that the relay
    /// took the command should not wait for it. The two are separate calls, so
    /// the cheap one can answer first.
    #[cfg(unix)]
    #[tokio::test]
    async fn submitting_does_not_catch_the_projection_up_until_asked() {
        if std::env::var_os(SUBMIT_WITHOUT_SYNC_TEST_CHILD).is_none() {
            run_in_isolated_child(
                SUBMIT_WITHOUT_SYNC_TEST_CHILD,
                "submitting_does_not_catch_the_projection_up_until_asked",
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::hel_database::install_isolated_test_writer();
        register_leased_relay_session();
        let relay_root = tempfile::tempdir().unwrap();
        let mut connection = StandaloneSession::connect(&leased_relay_target(relay_root.path()))
            .await
            .expect("connect to the live test relay");
        let before = connection.materialized.applied_event_ordinal;

        let ordinal = connection
            .submit_accepted(
                new_command_id("prompt").unwrap(),
                RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
                },
            )
            .await
            .expect("the relay accepted the command");
        assert!(ordinal > before, "the relay reported where it accepted it");
        assert_eq!(
            connection.materialized.applied_event_ordinal, before,
            "the caller was answered without paying for the catch-up"
        );

        connection.sync().await.expect("catch the projection up");
        assert!(
            connection.materialized.applied_event_ordinal > before,
            "the catch-up is what advances the projection"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresponsive_live_relay_worker_is_restarted_and_reconnected() {
        if std::env::var_os(AUTO_RESTART_TEST_CHILD).is_none() {
            run_in_isolated_child(
                AUTO_RESTART_TEST_CHILD,
                "unresponsive_live_relay_worker_is_restarted_and_reconnected",
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::hel_database::install_isolated_test_writer();
        fail_if_the_actor_stalls("unresponsive live relay worker was never restarted");
        register_leased_relay_session();
        let relay_root = tempfile::tempdir().unwrap();
        let restarted = relay_root.path().join("worker-restarted");
        let script = format!(
            "if [ ! -f \"${AUTO_RESTART_MARKER}\" ]; then IFS= read -r _; exit 0; fi; \
             \"$0\" --exact {} --nocapture | grep --line-buffered '^{{'",
            exact_test_name("leased_relay_child_serves_stdio")
        );
        let mut spec = CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                script,
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ],
        )
        .purpose("test restartable relay");
        spec.env.insert(
            LEASED_RELAY_ROOT.to_owned(),
            relay_root.path().to_string_lossy().into_owned(),
        );
        spec.env.insert(
            AUTO_RESTART_MARKER.to_owned(),
            restarted.to_string_lossy().into_owned(),
        );
        let worker_recovery = WorkerRecoveryPlan {
            target: None,
            liveness_probe: CommandSpec::new("printf", ["alive\n"])
                .purpose("probe test relay worker"),
            binary_refresh: None,
            launch_refresh: None,
            restart: CommandPlan {
                description: "restart test relay worker".into(),
                commands: vec![
                    CommandSpec::new("touch", [restarted.to_string_lossy().into_owned()])
                        .purpose("restart test relay worker"),
                ],
            },
        };
        let target = RelaySessionTarget {
            session_id: LEASED_RELAY_SESSION.to_owned(),
            spec,
            worker_recovery: Some(worker_recovery),
            project_memory: None,
        };
        let (_commands_tx, commands_rx) = mpsc::channel(4);
        let (_releases_tx, releases_rx) = mpsc::unbounded_channel();
        let (_retirement_tx, retirement_rx) = watch::channel(false);
        let (view_tx, mut view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, _updates_rx) = coalesced_update_channel();
        tokio::spawn(run_session_actor(
            target,
            commands_rx,
            releases_rx,
            retirement_rx,
            view_tx,
            updates_tx,
        ));

        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                view_rx.changed().await.unwrap();
                let view = view_rx.borrow_and_update().clone();
                if view.connected {
                    assert!(restarted.exists(), "the restart plan did not run");
                    assert!(view.error.is_none());
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("relay stayed disconnected: {:?}", view_rx.borrow().error));
    }

    /// A deferred submission that is never answered would hang the suite
    /// instead of failing it, so turn a stall into a hard error.
    #[cfg(unix)]
    fn fail_if_the_actor_stalls(reason: &'static str) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            eprintln!("{reason}");
            std::process::exit(101);
        });
    }

    /// A relay target served by this test binary over stdio.
    #[cfg(unix)]
    fn leased_relay_target(relay_root: &std::path::Path) -> RelaySessionTarget {
        // `RelayClient` parses every stdout line as JSON, so libtest's own
        // progress lines are dropped before they reach the protocol reader.
        let script = format!(
            "\"$0\" --exact {} --nocapture | grep --line-buffered '^{{'",
            exact_test_name("leased_relay_child_serves_stdio")
        );
        let mut spec = CommandSpec::new(
            "sh",
            [
                "-c".to_owned(),
                script,
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ],
        )
        .purpose("test leased relay");
        spec.env.insert(
            LEASED_RELAY_ROOT.to_owned(),
            relay_root.to_string_lossy().into_owned(),
        );
        RelaySessionTarget {
            session_id: LEASED_RELAY_SESSION.to_owned(),
            spec,
            worker_recovery: None,
            project_memory: None,
        }
    }

    /// Register the session the projection writes to. `apply_projection_event`
    /// rejects events for sessions the controller database does not know.
    #[cfg(unix)]
    fn register_leased_relay_session() {
        crate::hel_database::save_session(&crate::hel_state::SessionRecord {
            workspace_id: crate::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: LEASED_RELAY_SESSION.into(),
            title: "leased relay".into(),
            harness_kind: crate::hel_config::HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: crate::hel_state::SessionState::Running,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-08-12T00:00:00Z".into(),
            updated_at: "2026-08-12T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        })
        .expect("register the test session");
    }

    #[cfg(unix)]
    struct LeasedActor {
        commands: mpsc::Sender<ActorCommand>,
        releases: mpsc::UnboundedSender<ReturnedConnection>,
        retirement: watch::Sender<bool>,
        _views: watch::Receiver<ManagedSessionView>,
        _updates: SessionManagerUpdates,
        _relay_root: tempfile::TempDir,
    }

    /// Start an actor against a live relay and take its connection under lease.
    #[cfg(unix)]
    async fn lease_a_live_actor() -> (LeasedActor, u64, StandaloneSession) {
        register_leased_relay_session();
        let relay_root = tempfile::tempdir().unwrap();
        let (commands_tx, commands_rx) = mpsc::channel(4);
        let (releases_tx, releases_rx) = mpsc::unbounded_channel();
        let (retirement_tx, retirement_rx) = watch::channel(false);
        let (view_tx, view_rx) = watch::channel(ManagedSessionView::default());
        let (updates_tx, updates_rx) = coalesced_update_channel();
        tokio::spawn(run_session_actor(
            leased_relay_target(relay_root.path()),
            commands_rx,
            releases_rx,
            retirement_rx,
            view_tx,
            updates_tx,
        ));

        let (reply, response) = oneshot::channel();
        commands_tx
            .send(ActorCommand::Lease { reply })
            .await
            .unwrap();
        let (lease_id, connection) = response
            .await
            .expect("actor answered the lease request")
            .expect("actor leased its relay connection");
        (
            LeasedActor {
                commands: commands_tx,
                releases: releases_tx,
                retirement: retirement_tx,
                _views: view_rx,
                _updates: updates_rx,
                _relay_root: relay_root,
            },
            lease_id,
            connection,
        )
    }

    #[cfg(unix)]
    async fn submit_a_deferred_prompt(
        actor: &LeasedActor,
    ) -> oneshot::Receiver<std::result::Result<u64, String>> {
        let (reply, mut response) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::Submit {
                command_id: new_command_id("prompt").unwrap(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
                },
                reply,
            })
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut response)
                .await
                .is_err(),
            "a leased actor must hold the prompt instead of answering it"
        );
        response
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prompt_submitted_during_lease_is_delivered_after_release() {
        if std::env::var_os(DEFERRED_SUBMIT_TEST_CHILD).is_none() {
            run_in_isolated_child(
                DEFERRED_SUBMIT_TEST_CHILD,
                "prompt_submitted_during_lease_is_delivered_after_release",
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::hel_database::install_isolated_test_writer();
        fail_if_the_actor_stalls("prompt deferred during a lease was never delivered");

        let (actor, lease_id, connection) = lease_a_live_actor().await;
        let response = submit_a_deferred_prompt(&actor).await;

        actor
            .releases
            .send(ReturnedConnection {
                lease_id,
                connection: Some(connection),
            })
            .unwrap();

        let ordinal = response
            .await
            .expect("actor answered the deferred prompt")
            .expect("deferred prompt reached the relay");
        assert!(
            ordinal > 0,
            "relay accepted the prompt at ordinal {ordinal}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn returned_lease_publishes_what_it_learned_while_it_held_the_connection() {
        if std::env::var_os(RETURNED_LEASE_VIEW_TEST_CHILD).is_none() {
            run_in_isolated_child(
                RETURNED_LEASE_VIEW_TEST_CHILD,
                "returned_lease_publishes_what_it_learned_while_it_held_the_connection",
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::hel_database::install_isolated_test_writer();
        fail_if_the_actor_stalls("a returned lease never republished its session");

        let (actor, lease_id, mut connection) = lease_a_live_actor().await;
        let mut views = actor._views.clone();
        // The lease applies these events itself, so the actor's own next sync
        // has nothing left to catch up on.
        let ordinal = connection
            .submit(
                new_command_id("prompt").unwrap(),
                RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("hello"))],
                },
            )
            .await
            .unwrap();
        assert!(views.borrow_and_update().snapshot.is_none());

        actor
            .releases
            .send(ReturnedConnection {
                lease_id,
                connection: Some(connection),
            })
            .unwrap();

        views.changed().await.unwrap();
        let snapshot = views
            .borrow_and_update()
            .snapshot
            .clone()
            .expect("the returned connection republished its session");
        assert!(
            snapshot.materialized.applied_event_ordinal >= ordinal,
            "published frontier {} is behind the leased submission at {ordinal}",
            snapshot.materialized.applied_event_ordinal
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retirement_rejects_prompts_deferred_during_lease() {
        if std::env::var_os(RETIRED_SUBMIT_TEST_CHILD).is_none() {
            run_in_isolated_child(
                RETIRED_SUBMIT_TEST_CHILD,
                "retirement_rejects_prompts_deferred_during_lease",
            );
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::hel_database::install_isolated_test_writer();
        fail_if_the_actor_stalls("prompt deferred during a lease was never answered");

        let (actor, lease_id, connection) = lease_a_live_actor().await;
        let response = submit_a_deferred_prompt(&actor).await;

        actor.retirement.send(true).unwrap();
        actor
            .releases
            .send(ReturnedConnection {
                lease_id,
                connection: Some(connection),
            })
            .unwrap();

        let error = response
            .await
            .expect("actor answered the deferred prompt")
            .expect_err("a retiring actor must not deliver the prompt");
        assert!(
            error.contains("session target is changing"),
            "unexpected rejection: {error}"
        );
    }

    #[test]
    fn projection_integrity_failure_is_detected_only_for_integrity_errors() {
        let integrity = anyhow::Error::from(ProjectionIntegrityError(
            "transcript item \"tool:call-1\" changed immutable identity fields".into(),
        ))
        .context("apply projection event");
        assert!(projection_integrity_failure(&integrity));

        let concurrent = anyhow::Error::from(ProjectionAdvancedError { event_ordinal: 7 });
        assert!(!projection_integrity_failure(&concurrent));

        let unreachable = anyhow::anyhow!("connection refused").context("connect relay proxy");
        assert!(!projection_integrity_failure(&unreachable));
    }

    #[test]
    fn dashboard_updates_keep_only_the_latest_view_per_session() {
        let (sender, mut receiver) = coalesced_update_channel();
        for revision in 0..1_000 {
            sender.send(SessionManagerUpdate {
                session_id: "session-1".into(),
                view: ManagedSessionView {
                    error: Some(ViewError::Unreachable(format!("revision-{revision}"))),
                    ..ManagedSessionView::default()
                },
            });
        }
        sender.send(SessionManagerUpdate {
            session_id: "session-2".into(),
            view: ManagedSessionView {
                error: Some(ViewError::Unreachable("other".into())),
                ..ManagedSessionView::default()
            },
        });

        assert_eq!(
            sender
                .pending
                .lock()
                .expect("session update coalescer poisoned")
                .len(),
            2
        );
        let updates = [receiver.try_recv().unwrap(), receiver.try_recv().unwrap()]
            .into_iter()
            .map(|update| (update.session_id, update.view.error.unwrap()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(updates["session-1"].detail(), "revision-999");
        assert_eq!(updates["session-2"].detail(), "other");
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn remote_session_manager_fans_out_views_and_forwards_commands() {
        let mut remote = spawn_remote_session_manager().unwrap();
        remote.targets.send_replace(vec![target("unused")]);
        remote
            .publisher
            .publish("session-1".into(), view_at_ordinal(7))
            .await
            .unwrap();

        let session = remote
            .control
            .wait_for_session("session-1", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            session
                .view()
                .snapshot
                .as_ref()
                .unwrap()
                .materialized
                .applied_event_ordinal,
            7
        );

        let submitted = session
            .enqueue_submit("prompt-1".into(), RelayCommand::Cancel)
            .await
            .unwrap();
        let request = remote.requests.recv().await.unwrap();
        match request {
            RemoteSessionRequest::Submit {
                session_id,
                command_id,
                command: RelayCommand::Cancel,
                reply,
            } => {
                assert_eq!(session_id, "session-1");
                assert_eq!(command_id, "prompt-1");
                reply.send(Ok(8)).unwrap();
            }
            _ => panic!("unexpected remote session request"),
        }
        assert_eq!(submitted.wait().await.unwrap(), 8);
        remote.shutdown.shutdown().await.unwrap();
    }
}
