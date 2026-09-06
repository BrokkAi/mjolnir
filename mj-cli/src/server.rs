//! The phone-oriented remote-control server: its HTTP surface, the controller
//! actions phones request, and the concurrency limits that keep them safe.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hel::hel_config::{HarnessProfile, HelConfig, PhoneConfig, is_bare_project_target};
use hel::hel_state::{HelState, MaterializedSession, SessionRecord};
use hel::hel_targets::{CancellableProcessExecutor, CommandExecutor, ProcessExecutor};
use hel::hel_worker::RelayCommand;
use hel::hel_workspace::WorkspaceRecord;
use mj_controller::hel_controller::{Controller, SessionLaunchOptions};
use mj_controller::hel_quota::ProfileQuota;
use mj_controller::hel_server::{
    ActionOutcome, BrowserTranscript, ControllerAction, ControllerRequest, PreflightFailure,
    ReadReceiptRequest, ResumeQueueDisposition, ServerOptions, ViewerQueuedPrompt, ViewerQuota,
    ViewerSnapshot, ViewerUserShell,
};
use mj_controller::hel_session_manager::{
    SessionManagerChannels, SessionManagerControl, new_command_id,
};
use mj_controller::hel_tailscale::TailscaleTls;
use mj_controller::hel_worker_client::CredentialSyncCoordinator;

use crate::daemon::{ResumeSessionRequest, RuntimeState, WebViewerStatus};
use crate::dashboard::io::config_only_controller;
use crate::pollers::{
    CredentialSyncNotices, CredentialSyncSignalTracker, QUOTA_STALE_AFTER, QuotaRefreshBatch,
    QuotaUpdate, apply_worker_record_update, credential_sync_targets, dashboard_worker_targets,
    projected_queued_prompts, queued_prompt_projection, quota_refresh_profiles,
    schedule_due_credential_syncs, spawn_quota_refresher,
};

#[derive(Debug, Clone)]
pub(crate) struct ServerArgs {
    bind: String,
    tailscale_detect: bool,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
}

impl From<&PhoneConfig> for ServerArgs {
    fn from(config: &PhoneConfig) -> Self {
        Self {
            bind: config.bind.clone(),
            tailscale_detect: config.tailscale_detect,
            tls_cert: config.tls_cert.clone(),
            tls_key: config.tls_key.clone(),
        }
    }
}

const TAILSCALE_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const TAILSCALE_RENEW_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Transcript projection parses every stored ACP content chunk. Keep that work
/// off the control task, and allow only a small number of projections to use
/// the blocking pool at once so a burst of active sessions cannot turn the
/// pool into an unbounded queue.
const MAX_CONCURRENT_CONVERSATION_PROJECTIONS: usize = 2;
const CONVERSATION_PROJECTION_CHANNEL_CAPACITY: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConversationProjectionKey {
    ordinal: u64,
    digest: String,
}

impl ConversationProjectionKey {
    fn of(materialized: &MaterializedSession) -> Self {
        Self {
            ordinal: materialized.applied_event_ordinal,
            digest: materialized.applied_event_digest.clone(),
        }
    }

    /// Event ordinals are monotonic. A changed digest at one ordinal is also a
    /// new projection, which preserves the integrity-repair path without
    /// relying on string ordering for digests.
    fn is_newer_than(&self, other: &Self) -> bool {
        self.ordinal > other.ordinal
            || (self.ordinal == other.ordinal && self.digest != other.digest)
    }
}

struct ConversationProjectionRequest {
    materialized: MaterializedSession,
    key: ConversationProjectionKey,
    generation: u64,
}

struct ConversationProjectionResult {
    session_id: String,
    key: ConversationProjectionKey,
    generation: u64,
    result: std::result::Result<BrowserTranscript, String>,
}

/// Owns the one-at-a-time/latest-state scheduling for each session. The
/// control loop remains the sole owner of these maps; background tasks only
/// return completed browser projections through `results`.
struct ConversationProjectionDispatcher {
    in_flight: std::collections::BTreeMap<String, (ConversationProjectionKey, u64)>,
    pending: std::collections::BTreeMap<String, ConversationProjectionRequest>,
    completed: std::collections::BTreeMap<String, ConversationProjectionKey>,
    generations: std::collections::BTreeMap<String, u64>,
    permits: Arc<tokio::sync::Semaphore>,
    results: tokio::sync::mpsc::Sender<ConversationProjectionResult>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl ConversationProjectionDispatcher {
    fn new(
        results: tokio::sync::mpsc::Sender<ConversationProjectionResult>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            in_flight: std::collections::BTreeMap::new(),
            pending: std::collections::BTreeMap::new(),
            completed: std::collections::BTreeMap::new(),
            generations: std::collections::BTreeMap::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(
                MAX_CONCURRENT_CONVERSATION_PROJECTIONS,
            )),
            results,
            shutdown,
        }
    }

    #[cfg(test)]
    fn with_permits(
        results: tokio::sync::mpsc::Sender<ConversationProjectionResult>,
        shutdown: tokio_util::sync::CancellationToken,
        permits: usize,
    ) -> Self {
        let mut dispatcher = Self::new(results, shutdown);
        dispatcher.permits = Arc::new(tokio::sync::Semaphore::new(permits));
        dispatcher
    }

    /// Queue a session's newest durable view. At most one request is running
    /// and one newer request is retained for any given session.
    fn enqueue(&mut self, materialized: MaterializedSession) {
        let session_id = materialized.session_id.clone();
        let key = ConversationProjectionKey::of(&materialized);
        if self
            .completed
            .get(&session_id)
            .is_some_and(|completed| !key.is_newer_than(completed))
        {
            return;
        }
        let generation = *self.generations.entry(session_id.clone()).or_default();
        if let Some((in_flight, in_flight_generation)) = self.in_flight.get(&session_id) {
            if generation != *in_flight_generation || key.is_newer_than(in_flight) {
                let replace = self.pending.get(&session_id).is_none_or(|pending| {
                    pending.generation != generation || key.is_newer_than(&pending.key)
                });
                if replace {
                    self.pending.insert(
                        session_id,
                        ConversationProjectionRequest {
                            materialized,
                            key,
                            generation,
                        },
                    );
                }
            }
            return;
        }
        self.in_flight
            .insert(session_id.clone(), (key.clone(), generation));
        self.start(ConversationProjectionRequest {
            materialized,
            key,
            generation,
        });
    }

    /// Finish one task and, when the session is still active, immediately
    /// launch the newest coalesced request. Returning `None` means the task
    /// failed or no longer belongs to the current in-flight request.
    fn finish(
        &mut self,
        result: ConversationProjectionResult,
        session_active: bool,
    ) -> Option<(String, ConversationProjectionKey, BrowserTranscript)> {
        let expected = self.in_flight.remove(&result.session_id);
        if expected.as_ref() != Some(&(result.key.clone(), result.generation)) {
            tracing::warn!(
                session_id = %result.session_id,
                "discarding an out-of-date browser transcript projection"
            );
            return None;
        }
        let session_id = result.session_id;
        let key = result.key;
        let current_generation = self
            .generations
            .get(&session_id)
            .copied()
            .unwrap_or_default();
        let current = result.generation == current_generation;
        let projected = match result.result {
            Ok(transcript) if session_active && current => {
                self.completed.insert(session_id.clone(), key.clone());
                Some((session_id.clone(), key, transcript))
            }
            Ok(_) => {
                // An inactive session, or a result from an earlier lifecycle
                // generation, must not resurrect a conversation. Its next
                // active update gets a fresh generation.
                self.completed.remove(&session_id);
                None
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    "browser transcript projection failed: {error}"
                );
                None
            }
        };
        if session_active {
            if let Some(pending) = self.pending.remove(&session_id) {
                self.enqueue(pending.materialized);
            }
        } else {
            self.pending.remove(&session_id);
            self.completed.remove(&session_id);
        }
        projected
    }

    /// Drop queued/completed state after a controller reload removes a
    /// session. An in-flight task is allowed to finish; `finish` receives the
    /// current active-state guard and discards its result.
    fn forget(&mut self, session_id: &str) {
        self.pending.remove(session_id);
        self.completed.remove(session_id);
        let generation = self.generations.entry(session_id.to_owned()).or_default();
        *generation = generation.wrapping_add(1);
    }

    fn session_ids(&self) -> std::collections::BTreeSet<String> {
        self.in_flight
            .keys()
            .chain(self.pending.keys())
            .chain(self.completed.keys())
            .cloned()
            .collect()
    }

    fn start(&self, request: ConversationProjectionRequest) {
        let session_id = request.materialized.session_id.clone();
        let key = request.key;
        let generation = request.generation;
        let permits = Arc::clone(&self.permits);
        let results = self.results.clone();
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let result = match tokio::select! {
                _ = shutdown.cancelled() => return,
                result = permits.acquire_owned() => result,
            } {
                Ok(permit) => {
                    let projection = tokio::task::spawn_blocking(move || {
                        mj_chat::hel_chat::TranscriptSnapshot::from_materialized(
                            &request.materialized,
                        )
                        .browser_transcript(None)
                    })
                    .await;
                    drop(permit);
                    projection
                        .map_err(|error| format!("transcript projection task failed: {error}"))
                }
                Err(error) => Err(format!("transcript projection worker stopped: {error}")),
            };
            let message = ConversationProjectionResult {
                session_id,
                key,
                generation,
                result,
            };
            tokio::select! {
                _ = shutdown.cancelled() => {}
                result = results.send(message) => {
                    if let Err(error) = result {
                        tracing::debug!(%error, "browser transcript projection result dropped after server shutdown");
                    }
                }
            }
        });
    }
}

struct ResolvedServerArgs {
    bind: SocketAddr,
    viewer_url: String,
    tls_files: Option<(PathBuf, PathBuf)>,
    tailscale: Option<TailscaleTls>,
    fallback_reason: Option<String>,
}

async fn resolve_server_args(
    args: ServerArgs,
    termination: tokio_util::sync::CancellationToken,
) -> Result<ResolvedServerArgs> {
    let configured_bind: SocketAddr = args.bind.parse().context("parse web viewer bind address")?;
    match (args.tls_cert, args.tls_key) {
        (Some(cert), Some(key)) => {
            let scheme = "https";
            return Ok(ResolvedServerArgs {
                bind: configured_bind,
                viewer_url: format!("{scheme}://{configured_bind}"),
                tls_files: Some((cert, key)),
                tailscale: None,
                fallback_reason: None,
            });
        }
        (None, None) => {}
        _ => bail!("web viewer TLS requires both a certificate and private key"),
    }

    if !args.tailscale_detect {
        return Ok(loopback_server_args(
            configured_bind,
            Some("automatic Tailscale detection is disabled".into()),
        ));
    }

    let tls_root = hel::hel_config::data_dir().join("viewer");
    let prepared = run_tailscale_blocking(termination.clone(), move |executor| {
        mj_controller::hel_tailscale::prepare_tailscale_tls(&tls_root, executor)
    })
    .await;
    match prepared {
        Ok(tailscale) => {
            let bind = tailscale_bind(configured_bind);
            let viewer_url = format!(
                "https://{}:{}",
                tailscale.cert_domain(),
                configured_bind.port()
            );
            Ok(ResolvedServerArgs {
                bind,
                viewer_url,
                tls_files: Some((
                    tailscale.cert_path().to_owned(),
                    tailscale.key_path().to_owned(),
                )),
                tailscale: Some(tailscale),
                fallback_reason: None,
            })
        }
        Err(error) if termination.is_cancelled() => Err(error),
        Err(error) => {
            let reason = format!("{error:#}");
            tracing::debug!(error = reason, "Tailscale HTTPS unavailable for web viewer");
            Ok(loopback_server_args(configured_bind, Some(reason)))
        }
    }
}

fn tailscale_bind(configured_bind: SocketAddr) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::UNSPECIFIED, configured_bind.port()))
}

fn loopback_server_args(bind: SocketAddr, fallback_reason: Option<String>) -> ResolvedServerArgs {
    ResolvedServerArgs {
        bind,
        viewer_url: format!("http://{bind}"),
        tls_files: None,
        tailscale: None,
        fallback_reason,
    }
}

async fn run_tailscale_blocking<T>(
    termination: tokio_util::sync::CancellationToken,
    operation: impl FnOnce(&CancellableProcessExecutor) -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    let executor_cancelled = cancelled.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        let executor = CancellableProcessExecutor::new(executor_cancelled)
            .with_deadline(TAILSCALE_COMMAND_TIMEOUT);
        operation(&executor)
    });
    tokio::select! {
        result = &mut task => result.context("Tailscale background task panicked")?,
        _ = termination.cancelled() => {
            cancelled.store(true, Ordering::Release);
            let _ = task.await;
            bail!("Tailscale operation cancelled during web viewer shutdown")
        }
    }
}

fn spawn_tailscale_cert_renewer(
    tailscale: TailscaleTls,
    rustls: axum_server::tls_rustls::RustlsConfig,
    termination: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TAILSCALE_RENEW_INTERVAL);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = termination.cancelled() => return,
                _ = interval.tick() => {}
            }
            let renewing = tailscale.clone();
            let result = run_tailscale_blocking(termination.clone(), move |executor| {
                renewing.renew(executor)
            })
            .await;
            if let Err(error) = result {
                if !termination.is_cancelled() {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "Tailscale certificate renewal failed"
                    );
                }
                continue;
            }
            if let Err(error) = rustls
                .reload_from_pem_file(tailscale.cert_path(), tailscale.key_path())
                .await
            {
                tracing::warn!(%error, "could not activate renewed Tailscale certificate");
            }
        }
    })
}

const MAX_CONCURRENT_PHONE_ACTIONS: usize = 4;
const MAX_CONCURRENT_BUNDLE_CREATIONS: usize = 4;

struct PhoneActionStarted {
    action_id: u64,
    session: SessionRecord,
    published: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
}

/// The phone replies the control loop still owes.
///
/// A phone is answered as soon as its action is admitted, because provisioning,
/// resume and close run for minutes and a request held open that long dies on a
/// mobile network. `new` is the one action whose acceptance means more than
/// admission: the phone has no session id until the provisional session is
/// published, so its reply is parked here until the loop publishes it — or
/// until the action ends without ever getting that far.
#[derive(Default)]
struct PendingActionReplies(
    std::collections::BTreeMap<u64, tokio::sync::oneshot::Sender<ActionOutcome>>,
);

impl PendingActionReplies {
    fn accept(
        &mut self,
        action_id: u64,
        action: &ControllerAction,
        reply: tokio::sync::oneshot::Sender<ActionOutcome>,
    ) {
        if matches!(action, ControllerAction::New { .. }) {
            self.0.insert(action_id, reply);
        } else {
            if reply.send(ActionOutcome::Accepted).is_err() {
                tracing::debug!(
                    action_id,
                    "phone action acceptance reply dropped after client disconnect"
                );
            }
        }
    }

    fn resolve(&mut self, action_id: u64, outcome: ActionOutcome) {
        if let Some(reply) = self.0.remove(&action_id)
            && reply.send(outcome).is_err()
        {
            tracing::debug!(
                action_id,
                "phone action completion reply dropped after client disconnect"
            );
        }
    }
}

/// Admission control for one phone action, run before any work starts so the
/// answer to the phone never waits on the operation itself. Reports the session
/// the action occupies, or the outcome that refuses it.
fn admit_phone_action(
    action: &ControllerAction,
    running_actions: usize,
    active_sessions: &mut std::collections::BTreeSet<String>,
) -> std::result::Result<Option<String>, ActionOutcome> {
    if !phone_action_capacity_available(running_actions) {
        return Err(ActionOutcome::Busy);
    }
    let session_id = controller_action_session_id(action);
    if let Some(session_id) = &session_id
        && !active_sessions.insert(session_id.clone())
    {
        return Err(ActionOutcome::SessionBusy);
    }
    Ok(session_id)
}

struct ReadReceiptPersisted {
    session_id: String,
    result: std::result::Result<u64, String>,
    reply: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
}

struct ControllerReloaded {
    result: std::result::Result<Controller, String>,
}

struct BundleCreated {
    result: std::result::Result<
        mj_controller::hel_controller::QuickBundleCreation,
        mj_controller::hel_controller::QuickBundleFailure,
    >,
    reply: tokio::sync::oneshot::Sender<
        std::result::Result<String, mj_controller::hel_server::BundleFailure>,
    >,
}

/// Loads durable controller state without occupying the phone control loop.
/// The outer task observes blocking-task panics and reports a closed result
/// channel instead of silently abandoning the refresh.
fn spawn_controller_reload(completed: tokio::sync::mpsc::UnboundedSender<ControllerReloaded>) {
    spawn_controller_reload_with(completed, Controller::load);
}

fn spawn_controller_reload_with(
    completed: tokio::sync::mpsc::UnboundedSender<ControllerReloaded>,
    load: impl FnOnce() -> Result<Controller> + Send + 'static,
) {
    tokio::spawn(async move {
        let result = match tokio::task::spawn_blocking(load).await {
            Ok(result) => result.map_err(|error| format!("{error:#}")),
            Err(error) => Err(format!("controller reload task failed: {error}")),
        };
        if completed.send(ControllerReloaded { result }).is_err() {
            tracing::debug!("controller reload completed after the phone control loop stopped");
        }
    });
}

fn request_controller_reload(
    in_flight: &mut bool,
    requested: &mut bool,
    completed: &tokio::sync::mpsc::UnboundedSender<ControllerReloaded>,
) {
    if *in_flight {
        *requested = true;
    } else {
        *in_flight = true;
        spawn_controller_reload(completed.clone());
    }
}

fn request_daemon_controller_reload(daemon_runtime: Arc<RuntimeState>, reason: &'static str) {
    tokio::spawn(async move {
        if let Err(error) = daemon_runtime.reload_controller().await {
            tracing::warn!(
                error = format!("{error:#}"),
                reason,
                "phone operation could not refresh dashboard controller state"
            );
        }
    });
}

/// What one phone read receipt actually needs.
#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
enum ReadReceiptPlan {
    UnknownSession,
    /// The cursor has not advanced, so the receipt needs no work at all.
    AlreadyRead,
    /// The cursor advanced: persist it, then refresh the snapshot.
    Persist,
}

#[cfg(test)]
fn plan_read_receipt(state: &HelState, session_id: &str, through: u64) -> ReadReceiptPlan {
    let Some(session) = state.sessions.get(session_id) else {
        return ReadReceiptPlan::UnknownSession;
    };
    if through > session.viewed_through_event_ordinal {
        ReadReceiptPlan::Persist
    } else {
        ReadReceiptPlan::AlreadyRead
    }
}

/// Record a persisted receipt in the in-memory projection, reporting whether
/// the cursor moved. That is exactly when the snapshot revision has to move,
/// so surfaces showing unread state refresh and nothing else does.
#[cfg(test)]
fn apply_read_receipt(state: &mut HelState, session_id: &str, receipt: u64) -> bool {
    let Some(session) = state.sessions.get_mut(session_id) else {
        return false;
    };
    if receipt <= session.viewed_through_event_ordinal {
        return false;
    }
    session.viewed_through_event_ordinal = receipt;
    true
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum PhoneNewActionState {
    Active = 0,
    CancelRequested = 1,
    CommitGranted = 2,
}

struct PhoneNewActionGate {
    state: AtomicU8,
}

impl PhoneNewActionGate {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(PhoneNewActionState::Active as u8),
        }
    }

    fn request_cancel(&self) -> bool {
        self.state
            .compare_exchange(
                PhoneNewActionState::Active as u8,
                PhoneNewActionState::CancelRequested as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn grant_commit(&self) -> bool {
        self.state
            .compare_exchange(
                PhoneNewActionState::Active as u8,
                PhoneNewActionState::CommitGranted as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

#[derive(Clone)]
struct PhoneActionControl {
    cancelled: Arc<AtomicBool>,
    new_gate: Option<Arc<PhoneNewActionGate>>,
}

impl PhoneActionControl {
    fn for_action(action: &ControllerAction) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            new_gate: matches!(action, ControllerAction::New { .. })
                .then(|| Arc::new(PhoneNewActionGate::new())),
        }
    }

    fn request_cancel(&self) -> bool {
        let accepted = self.new_gate.as_ref().map_or_else(
            || {
                self.cancelled
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            },
            |gate| gate.request_cancel(),
        );
        if accepted {
            self.cancelled.store(true, Ordering::Release);
        }
        accepted
    }

    fn grant_new_commit(&self) -> bool {
        self.new_gate
            .as_ref()
            .is_some_and(|gate| gate.grant_commit())
    }
}

pub(crate) async fn run_server(
    args: ServerArgs,
    termination: tokio_util::sync::CancellationToken,
    report_status: impl FnOnce(WebViewerStatus),
    worker: SessionManagerChannels,
    daemon_runtime: Arc<RuntimeState>,
    mut workspace_updates: tokio::sync::watch::Receiver<Vec<WorkspaceRecord>>,
) -> Result<()> {
    let resolved = resolve_server_args(args, termination.clone()).await?;
    let bind = resolved.bind;
    let mut controller = Controller::load()?;
    let mut daemon_revisions = daemon_runtime.revisions();
    daemon_revisions.borrow_and_update();
    let mut phone_workspaces = workspace_updates.borrow_and_update().clone();
    let mut quotas = std::collections::BTreeMap::new();
    let (quota_profiles_tx, mut quota_updates_rx) = spawn_quota_refresher();
    let mut quota_batch = QuotaRefreshBatch::default();
    let mut published_quota_profiles = std::collections::BTreeMap::new();
    republish_quota_profiles(
        &controller,
        &mut published_quota_profiles,
        &mut quota_batch,
        &quota_profiles_tx,
    );
    let mut revision = daemon_runtime.allocate_revision();
    let mut conversations = std::collections::BTreeMap::new();
    let mut queued_prompts = projected_queued_prompts(&controller)?;
    let mut active_user_shells = std::collections::BTreeMap::new();
    let mut pending_elicitations = std::collections::BTreeMap::new();
    let mut prompt_images = std::collections::BTreeSet::new();
    let mut operational = std::collections::BTreeMap::new();
    let mut operations = std::collections::BTreeMap::new();
    let mut launch_failures = Vec::new();
    // What the capacity poller last said, per probe target. The projection is
    // built from this on every publish rather than being accumulated, so a
    // target that disappears from the configuration disappears from the page.
    let mut capacity_state: std::collections::BTreeMap<String, PhoneCapacity> =
        std::collections::BTreeMap::new();
    let (capacity_targets_tx, capacity_triggers_tx, mut capacity_updates_rx) =
        crate::pollers::spawn_dashboard_capacity_poller();
    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(viewer_snapshot(
        &controller,
        &phone_workspaces,
        &quotas,
        &PhoneSessionViews {
            conversations: &conversations,
            queued_prompts: &queued_prompts,
            active_user_shells: &active_user_shells,
            pending_elicitations: &pending_elicitations,
            prompt_images: &prompt_images,
            operational: &operational,
            operations: &operations,
            capacity: &viewer_capacity(&capacity_state),
            launch_failures: &launch_failures,
            reviews: &review_views(&daemon_runtime),
        },
        revision,
    ));
    let (conversation_tx, conversation_rx) = tokio::sync::watch::channel(conversations.clone());
    let (action_tx, mut action_rx) = tokio::sync::mpsc::channel(32);
    let (bundle_tx, mut bundle_rx) = tokio::sync::mpsc::channel(16);
    let (receipt_tx, mut receipt_rx) = tokio::sync::mpsc::channel(32);
    let (preflight_tx, mut preflight_rx) = tokio::sync::mpsc::channel(32);
    let (client_state_tx, mut client_state_rx) = tokio::sync::mpsc::channel(64);
    let SessionManagerChannels {
        targets: worker_targets_tx,
        control: worker_commands_tx,
        updates: mut worker_updates_rx,
        shutdown: worker_shutdown,
    } = worker;
    worker_targets_tx.send_replace(dashboard_worker_targets(&controller));
    publish_capacity_targets(&controller, &capacity_targets_tx, &mut capacity_state);
    let mut credential_sync = CredentialSyncCoordinator::spawn();
    let credential_sync_handle = credential_sync.handle();
    credential_sync_handle.set_targets(credential_sync_targets(&controller));
    let mut credential_sync_signals = CredentialSyncSignalTracker::default();
    let mut credential_sync_notices = CredentialSyncNotices::default();
    // Captured before `options` is moved into the server.
    let options_session_ttl = mj_controller::hel_server::default_session_ttl();
    let mut options = ServerOptions::new(
        bind,
        snapshot_rx,
        conversation_rx,
        mj_controller::hel_server::ServerRequests {
            action_tx,
            bundle_tx,
            receipt_tx,
            preflight_tx,
            client_state_tx,
        },
    )?;
    options.shutdown = termination.clone();
    // Session cookies are stateless, so a per-process key would sign every
    // phone out on every restart. Delete the key file to sign them out on
    // purpose.
    let cookie_key_path = mj_controller::hel_server::cookie_key_path();
    options.set_cookie_key(mj_controller::hel_server::load_or_create_cookie_key(
        &cookie_key_path,
    )?)?;
    let renewal_cancellation = termination.child_token();
    let mut renewal_task = None;
    if let Some((cert, key)) = resolved.tls_files {
        let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
            .await
            .context("load web viewer TLS certificate")?;
        options.set_tls_config(rustls.clone());
        if let Some(tailscale) = resolved.tailscale {
            renewal_task = Some(spawn_tailscale_cert_renewer(
                tailscale,
                rustls,
                renewal_cancellation.clone(),
            ));
        }
    } else if bind.ip().is_loopback() {
        options.secure_cookie = false;
    } else {
        anyhow::bail!("non-loopback web viewer requires TLS");
    }
    let fallback_reason = resolved.fallback_reason;
    let qr_login_url = if fallback_reason.is_none() && resolved.viewer_url.starts_with("https://") {
        let encoded = url::form_urlencoded::byte_serialize(options.login_token().as_bytes())
            .collect::<String>();
        Some(format!(
            "{}/auth/login?token={encoded}",
            resolved.viewer_url.trim_end_matches('/')
        ))
    } else {
        None
    };
    report_status(WebViewerStatus::Ready {
        viewer_url: resolved.viewer_url,
        viewer_code: options.viewer_code().to_owned(),
        qr_login_url,
        fallback_reason,
    });

    let serve = mj_controller::hel_server::run_server(options);
    let conversation_projection_shutdown = termination.child_token();
    let control = async {
        let mut credential_tick = tokio::time::interval(Duration::from_millis(250));
        // Stored viewer state expires with the authentication that created it.
        // The sweep is hourly rather than on every request, because it is
        // housekeeping and nothing waits for it.
        let mut prune_tick = tokio::time::interval(Duration::from_secs(60 * 60));
        let client_state_retention = options_session_ttl;
        let (action_done_tx, mut action_done_rx) = tokio::sync::mpsc::unbounded_channel::<(
            u64,
            Option<String>,
            std::result::Result<(), String>,
        )>();
        let (action_started_tx, mut action_started_rx) =
            tokio::sync::mpsc::unbounded_channel::<PhoneActionStarted>();
        let (receipt_done_tx, mut receipt_done_rx) =
            tokio::sync::mpsc::unbounded_channel::<ReadReceiptPersisted>();
        let (controller_reload_tx, mut controller_reload_rx) =
            tokio::sync::mpsc::unbounded_channel::<ControllerReloaded>();
        let (bundle_done_tx, mut bundle_done_rx) =
            tokio::sync::mpsc::unbounded_channel::<BundleCreated>();
        let mut bundle_jobs = tokio::task::JoinSet::new();
        let mut controller_reload_in_flight = false;
        let mut controller_reload_requested = false;
        let mut controller_reload_invalidated = false;
        let mut pending_action_errors = std::collections::BTreeMap::<String, String>::new();
        let mut active_actions = std::collections::BTreeSet::new();
        let mut next_action_id = 0_u64;
        let mut action_cancellations = std::collections::BTreeMap::<u64, PhoneActionControl>::new();
        let mut action_sessions = std::collections::BTreeMap::<u64, String>::new();
        let mut action_replies = PendingActionReplies::default();
        let mut launch_workspaces = std::collections::BTreeMap::new();
        let (conversation_projection_tx, mut conversation_projection_rx) =
            tokio::sync::mpsc::channel(CONVERSATION_PROJECTION_CHANNEL_CAPACITY);
        let mut conversation_projections = ConversationProjectionDispatcher::new(
            conversation_projection_tx,
            conversation_projection_shutdown.clone(),
        );
        let mut quota_updates_open = true;
        // A feed that ends is not a reason to exit quietly: the phone server
        // exists to follow sessions, so losing that feed is a named failure
        // rather than a silent success.
        let mut failure: Option<anyhow::Error> = None;
        macro_rules! publish_snapshot {
            ($revision:expr) => {
                if let Err(error) = snapshot_tx.send(viewer_snapshot(
                    &controller,
                    &phone_workspaces,
                    &quotas,
                    &PhoneSessionViews {
                        conversations: &conversations,
                        queued_prompts: &queued_prompts,
                        active_user_shells: &active_user_shells,
                        pending_elicitations: &pending_elicitations,
                        prompt_images: &prompt_images,
                        operational: &operational,
                        operations: &operations,
                        capacity: &viewer_capacity(&capacity_state),
                        launch_failures: &launch_failures,
                        reviews: &review_views(&daemon_runtime),
                    },
                    $revision,
                )) {
                    tracing::debug!(revision = $revision, %error, "phone snapshot delivery failed; no viewer is subscribed");
                }
            };
        }
        loop {
            tokio::select! {
                _ = termination.cancelled() => break,
                changed = daemon_revisions.changed() => {
                    if changed.is_err() {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the daemon stopped publishing runtime revisions to the phone server",
                        );
                        break;
                    }
                    daemon_revisions.borrow_and_update();
                    request_controller_reload(
                        &mut controller_reload_in_flight,
                        &mut controller_reload_requested,
                        &controller_reload_tx,
                    );
                }
                changed = workspace_updates.changed() => {
                    if changed.is_err() {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the daemon stopped publishing workspaces to the phone server",
                        );
                        break;
                    }
                    phone_workspaces = workspace_updates.borrow_and_update().clone();
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!(revision);
                }
                update = capacity_updates_rx.recv() => {
                    let Some(update) = update else {
                        failure = feed_stopped(termination.is_cancelled(), "the capacity poller stopped while the phone server was running");
                        break;
                    };
                    if let Some(entry) = capacity_state.get_mut(&update.target_id) {
                        entry.refreshing = false;
                        entry.sampled_at_epoch_seconds = Some(update.sampled_at_epoch_seconds);
                        match update.result {
                            Ok(usage) => {
                                // A fleet with nothing running reports no
                                // figures, and that is an answer rather than a
                                // failure.
                                entry.on_demand = usage.is_none();
                                entry.usage = usage;
                                entry.failed = false;
                            }
                            // The last good reading stays on screen beside the
                            // failure: one failed probe is not a reason to
                            // forget what the machine was doing.
                            Err(_) => entry.failed = true,
                        }
                    }
                    revision = daemon_runtime.allocate_revision();
                    publish_snapshot!(revision);
                }
                update = quota_updates_rx.recv(), if quota_updates_open => {
                    match update {
                        Some(QuotaUpdate::Report(outcome)) => {
                            if outcome.credentials_changed {
                                credential_sync_handle
                                    .sync_profile_now(&outcome.report.profile_id, None);
                            }
                            quotas.insert(outcome.report.profile_id.clone(), outcome.report);
                            revision = daemon_runtime.allocate_revision();
                            publish_snapshot!(revision);
                        }
                        Some(QuotaUpdate::Refreshing { .. } | QuotaUpdate::Finished { .. }) => {}
                        None => {
                            quota_updates_open = false;
                            tracing::warn!("quota refresher stopped while the phone server is running");
                        }
                    }
                }
                projected = conversation_projection_rx.recv() => {
                    let Some(projected) = projected else {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the browser transcript projection feed stopped",
                        );
                        break;
                    };
                    let session_id = projected.session_id.clone();
                    let session_active = controller
                        .state
                        .sessions
                        .get(&session_id)
                        .is_some_and(|session| session.state.is_active());
                    if !session_active {
                        // Invalidate a late result before it can be applied or
                        // launch another queued projection. A session that is
                        // resumed later gets a new generation from its next
                        // worker snapshot.
                        conversation_projections.forget(&session_id);
                    }
                    if let Some((session_id, _key, transcript)) =
                        conversation_projections.finish(projected, session_active)
                    {
                        conversations.insert(session_id, transcript);
                        revision = daemon_runtime.allocate_revision();
                        conversation_tx.send_replace(conversations.clone());
                        publish_snapshot!(revision);
                    } else if !session_active && conversations.remove(&session_id).is_some() {
                        // Controller reload normally removes inactive rows
                        // first, but this also covers a worker result racing
                        // that reload and keeps the viewer from seeing a
                        // conversation for a dead session.
                        revision = daemon_runtime.allocate_revision();
                        conversation_tx.send_replace(conversations.clone());
                        publish_snapshot!(revision);
                    }
                    // SessionManagerUpdates has a synchronous pending fast
                    // path. Yield after each completion so a hot stream of
                    // updates cannot monopolize this runtime worker.
                    tokio::task::yield_now().await;
                }
                update = worker_updates_rx.recv() => {
                    let Some(update) = update else {
                        failure = feed_stopped(termination.is_cancelled(), "the session manager stopped; the phone server can no longer follow sessions");
                        break;
                    };
                    if let Some(snapshot) = update.view.snapshot.as_ref()
                        && let Some(session) = controller.state.sessions.get(&update.session_id)
                        && let Some(signal) = snapshot.latest_credential_sync_signal.clone()
                    {
                        credential_sync_signals.observe(
                            &update.session_id,
                            &session.last_profile,
                            signal,
                        );
                    }
                    schedule_due_credential_syncs(
                        &mut credential_sync_signals,
                        &credential_sync_handle,
                        Instant::now(),
                    );
                    if let Err(error) =
                        apply_worker_record_update(&mut controller, &update, None)
                    {
                        tracing::warn!(session_id = %update.session_id, "could not persist relay session metadata: {error:#}");
                    }
                    if let Some(snapshot) = update.view.snapshot {
                        let materialized = snapshot.materialized;
                        let operational_state = snapshot.operational;
                        let queued = queued_prompt_projection(&materialized);
                        let pending = materialized.pending_elicitations.clone();
                        let active_shells = operational_state.active_user_shells.clone();
                        let prompt_images_supported =
                            agent_accepts_prompt_images(&operational_state);
                        active_user_shells.insert(
                            update.session_id.clone(),
                            active_shells,
                        );
                        conversation_projections.enqueue(materialized);
                        queued_prompts.insert(
                            update.session_id.clone(),
                            queued,
                        );
                        pending_elicitations.insert(
                            update.session_id.clone(),
                            pending,
                        );
                        if prompt_images_supported {
                            prompt_images.insert(update.session_id.clone());
                        } else {
                            prompt_images.remove(&update.session_id);
                        }
                        operational.insert(
                            update.session_id.clone(),
                            operational_state,
                        );
                        operations = daemon_runtime
                            .active_lifecycles()
                            .iter()
                            .map(|view| (view.session_id.clone(), viewer_operation(view)))
                            .collect();
                        revision = daemon_runtime.allocate_revision();
                        conversation_tx.send_replace(conversations.clone());
                        publish_snapshot!(revision);
                    }
                    // The session update receiver can return pending entries
                    // without touching Tokio's budgeted receive operation.
                    // Give HTTP/TLS tasks a scheduling opportunity after each
                    // update even when the worker is publishing continuously.
                    tokio::task::yield_now().await;
                }
                _ = prune_tick.tick() => {
                    // Only rows whose client id names a phone are considered:
                    // a terminal client's place in a conversation is not the
                    // phone's to expire.
                    tokio::spawn(async move {
                        let pruned = tokio::task::spawn_blocking(move || {
                            hel::hel_database::prune_phone_client_state(client_state_retention)
                        })
                        .await;
                        match pruned {
                            Ok(Ok(0)) => {}
                            Ok(Ok(rows)) => tracing::debug!(rows, "pruned expired phone viewer state"),
                            Ok(Err(error)) => tracing::warn!(%error, "could not prune phone viewer state"),
                            Err(error) => tracing::warn!(%error, "phone viewer state pruning task failed"),
                        }
                    });
                }
                _ = credential_tick.tick() => {
                    schedule_due_credential_syncs(
                        &mut credential_sync_signals,
                        &credential_sync_handle,
                        Instant::now(),
                    );
                    while let Some(result) = credential_sync.try_result() {
                        crate::pollers::log_credential_sync_actions(&result);
                        let harness = controller
                            .config
                            .profiles
                            .get(&result.profile_id)
                            .map(|profile| profile.kind);
                        if let Some(notice) = credential_sync_notices.notice(&result, harness) {
                            eprintln!("Mjolnir: {notice}");
                        }
                    }
                }
                stored = client_state_rx.recv() => {
                    let Some(stored) = stored else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering viewer state requests");
                        break;
                    };
                    // Every one of these touches SQLite, so each runs on its
                    // own task. A composer autosaving on a debounce must never
                    // be able to stall the loop that follows sessions.
                    let workspace_of = |session_id: &str| {
                        controller
                            .state
                            .sessions
                            .get(session_id)
                            .map(|session| session.workspace_id.clone())
                    };
                    let bundle_of = |session_id: &str| {
                        controller
                            .state
                            .sessions
                            .get(session_id)
                            .map(|session| session.bundle_id.clone())
                    };
                    match stored {
                        mj_controller::hel_server::ClientStateRequest::Read { client_id, session_id, reply } => {
                            let workspace = workspace_of(&session_id);
                            tokio::spawn(async move {
                                let answer = tokio::task::spawn_blocking(move || {
                                    let workspace = workspace.context("unknown session")?;
                                    let state = hel::hel_database::client_session_state(
                                        &client_id, &workspace, &session_id,
                                    )?;
                                    anyhow::Ok(mj_controller::hel_server::ViewerClientState {
                                        draft: state.draft,
                                        through_event_ordinal: state.through_event_ordinal,
                                    })
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                        mj_controller::hel_server::ClientStateRequest::SaveDraft { client_id, session_id, draft, reply } => {
                            let workspace = workspace_of(&session_id);
                            tokio::spawn(async move {
                                let answer = tokio::task::spawn_blocking(move || {
                                    let workspace = workspace.context("unknown session")?;
                                    hel::hel_database::persist_client_draft(
                                        &client_id, &workspace, &session_id, &draft,
                                    )
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                        mj_controller::hel_server::ClientStateRequest::MarkWorkspaceRead { client_id, workspace_id, reply } => {
                            let sessions = controller
                                .state
                                .sessions
                                .values()
                                .filter(|session| session.workspace_id == workspace_id)
                                .map(|session| (session.id.clone(), session.viewed_through_event_ordinal))
                                .collect::<Vec<_>>();
                            tokio::spawn(async move {
                                let answer = tokio::task::spawn_blocking(move || {
                                    for (session_id, through) in sessions {
                                        // A receipt that would move backwards
                                        // is not an error; it is a session this
                                        // viewer had already read past.
                                        hel::hel_database::persist_read_receipt(
                                            &client_id, &workspace_id, &session_id, through,
                                        )
                                        .ok();
                                    }
                                    anyhow::Ok(())
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                        mj_controller::hel_server::ClientStateRequest::History { session_id, query, scope, reply } => {
                            let bundle = bundle_of(&session_id);
                            tokio::spawn(async move {
                                let answer = tokio::task::spawn_blocking(move || {
                                    let bundle = bundle.context("unknown session")?;
                                    let scope = match scope.as_str() {
                                        "session" => hel::hel_database::HistoryScope::Session,
                                        "all" => hel::hel_database::HistoryScope::All,
                                        _ => hel::hel_database::HistoryScope::Project,
                                    };
                                    let found = hel::hel_database::search_prompts_bounded(
                                        &session_id,
                                        &bundle,
                                        scope,
                                        &query,
                                        mj_controller::hel_server::MAX_HISTORY_MATCHES,
                                    )?;
                                    anyhow::Ok(mj_controller::hel_server::ViewerPromptHistory {
                                        entries: found
                                            .entries
                                            .into_iter()
                                            .map(|entry| entry.text)
                                            .collect(),
                                        truncated: found.truncated,
                                    })
                                })
                                .await;
                                reply.send(flatten_stored(answer)).ok();
                            });
                        }
                    }
                }
                bundle = bundle_rx.recv(), if bundle_jobs.len() < MAX_CONCURRENT_BUNDLE_CREATIONS => {
                    let Some(mj_controller::hel_server::BundleRequest { source, reply }) = bundle else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering bundle requests");
                        break;
                    };
                    // Repository canonicalization and config persistence both
                    // touch the filesystem. Keep them off this loop, and
                    // report a panic as a failed request rather than dropping
                    // the browser's reply.
                    let done = bundle_done_tx.clone();
                    let daemon_runtime = daemon_runtime.clone();
                    bundle_jobs.spawn(async move {
                        let result = daemon_runtime
                            .create_quick_bundle(source)
                            .await;
                        if let Err(error) = done.send(BundleCreated { result, reply }) {
                            tracing::debug!(%error, "bundle creation finished after the server stopped");
                        }
                    });
                }
                bundle_done = bundle_done_rx.recv() => {
                    let Some(BundleCreated { result, reply }) = bundle_done else {
                        failure = feed_stopped(termination.is_cancelled(), "the bundle creation pipeline stopped while the phone server was running");
                        break;
                    };
                    match result {
                        Ok(created) => {
                            let bundle_id = created.bundle_id;
                            // Other config sections may have changed while
                            // this request was in flight. Publish only the
                            // bundle this transaction created; a full fresh
                            // config is requested below and must not make a
                            // later completion hide another completed bundle.
                            let Some(bundle) = created.config.bundles.get(&bundle_id) else {
                                tracing::error!(%bundle_id, "bundle creation returned a config without its bundle");
                                if reply.send(Err(mj_controller::hel_server::BundleFailure::Controller)).is_err() {
                                    tracing::debug!("bundle creation failure reply dropped after client disconnect");
                                }
                                continue;
                            };
                            controller
                                .config
                                .bundles
                                .insert(bundle_id.clone(), bundle.clone());
                            // A reload started before this save may still be
                            // queued. It must not hide a bundle after we have
                            // acknowledged it as available to the browser.
                            controller_reload_invalidated |= controller_reload_in_flight;
                            revision = daemon_runtime.allocate_revision();
                            publish_snapshot!(revision);
                            request_daemon_controller_reload(
                                daemon_runtime.clone(),
                                "new bundle publication",
                            );
                            if reply.send(Ok(bundle_id)).is_err() {
                                tracing::debug!("bundle creation reply dropped after client disconnect");
                            }
                        }
                        Err(error) => {
                            let failure = match error {
                                mj_controller::hel_controller::QuickBundleFailure::InvalidSource(
                                    detail,
                                ) => {
                                    tracing::debug!(error = %detail, "phone bundle source was invalid");
                                    mj_controller::hel_server::BundleFailure::InvalidSource
                                }
                                mj_controller::hel_controller::QuickBundleFailure::Persistence(
                                    detail,
                                ) => {
                                    tracing::warn!(error = %detail, "phone bundle creation failed");
                                    mj_controller::hel_server::BundleFailure::Controller
                                }
                            };
                            if reply.send(Err(failure)).is_err() {
                                tracing::debug!("bundle creation failure reply dropped after client disconnect");
                            }
                        }
                    }
                }
                bundle_job = bundle_jobs.join_next(), if !bundle_jobs.is_empty() => {
                    if let Some(Err(error)) = bundle_job {
                        tracing::warn!(%error, "bundle creation task panicked");
                    }
                }
                preflight = preflight_rx.recv() => {
                    let Some(mj_controller::hel_server::PreflightRequest {
                        bundle_id,
                        target_id,
                        project_directory,
                        reply,
                    }) = preflight else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering preflight requests");
                        break;
                    };
                    // Reading a working tree's status or validating a project
                    // directory touches the disk, so it runs on its own task
                    // rather than on the loop that has to stay responsive to
                    // every other feed.
                    let config = controller.config.clone();
                    let project_validation = project_directory.is_some();
                    tokio::spawn(async move {
                        let answer = tokio::task::spawn_blocking(move || {
                            run_new_preflight(config, bundle_id, target_id, project_directory)
                        })
                        .await;
                        let answer = match answer {
                            Ok(Ok(answer)) => Ok(answer),
                            Ok(Err(error)) => {
                                tracing::debug!(
                                    error = %error,
                                    project_validation,
                                    "phone preflight check failed"
                                );
                                Err(if project_validation {
                                    PreflightFailure::Validation
                                } else {
                                    PreflightFailure::Controller(format!("{error:#}"))
                                })
                            }
                            Err(error) => {
                                tracing::warn!(%error, "phone preflight task failed");
                                Err(PreflightFailure::Controller(format!(
                                    "preflight task failed: {error}"
                                )))
                            }
                        };
                        if reply.send(answer).is_err() {
                            tracing::debug!("phone preflight reply dropped after client disconnect");
                        }
                    });
                }
                receipt = receipt_rx.recv() => {
                    let Some(ReadReceiptRequest { client_id, session_id, through, reply }) = receipt else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering read receipts");
                        break;
                    };
                    match controller.state.sessions.get(&session_id) {
                        None => {
                            if reply.send(Err("unknown session".into())).is_err() {
                                tracing::debug!(%session_id, "unknown-session read receipt reply dropped after client disconnect");
                            }
                        }
                        Some(session) => {
                            let workspace_id = session.workspace_id.clone();
                            let done = receipt_done_tx.clone();
                            let persisted_session_id = session_id.clone();
                            tokio::spawn(async move {
                                let joined = tokio::task::spawn_blocking(move || {
                                    hel::hel_database::persist_read_receipt(
                                        &client_id,
                                        &workspace_id,
                                        &persisted_session_id,
                                        through,
                                    )
                                })
                                .await;
                                let result = match joined {
                                    Ok(result) => result.map_err(|error| format!("{error:#}")),
                                    Err(error) => Err(format!("phone read receipt task failed: {error}")),
                                };
                                if let Err(error) = done.send(ReadReceiptPersisted { session_id, result, reply }) {
                                    tracing::debug!(%error, "phone read receipt finished after the server stopped");
                                }
                            });
                        }
                    }
                }
                persisted = receipt_done_rx.recv() => {
                    let Some(ReadReceiptPersisted { session_id, result, reply }) = persisted else { continue };
                    match result {
                        Ok(receipt) => {
                            let _ = receipt;
                            if reply.send(Ok(())).is_err() {
                                tracing::debug!(%session_id, "phone read receipt reply dropped after client disconnect");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%session_id, "could not persist a phone read receipt: {error}");
                            if reply.send(Err(error)).is_err() {
                                tracing::debug!(%session_id, "failed phone read receipt reply dropped after client disconnect");
                            }
                        }
                    }
                }
                action = action_rx.recv() => {
                    let Some(request) = action else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone HTTP server stopped delivering actions");
                        break;
                    };
                    // A refresh nudges a poller this loop owns. It takes no
                    // session slot and starts no lifecycle work, so it is
                    // answered here rather than admitted as an action.
                    match &request.action {
                        ControllerAction::RefreshCapacity { target_id } => {
                            let known = capacity_state.contains_key(target_id);
                            // One queued nudge refreshes every target. Do not
                            // block the consumer while readings wait for it.
                            let accepted = known && match capacity_triggers_tx.try_send(()) {
                                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(())) => true,
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {
                                    tracing::warn!("phone capacity refresh rejected: poller stopped");
                                    false
                                }
                            };
                            if known {
                                if let Some(entry) = capacity_state.get_mut(target_id) {
                                    entry.refreshing = accepted;
                                    if !accepted {
                                        entry.failed = true;
                                    }
                                }
                                revision = daemon_runtime.allocate_revision();
                                publish_snapshot!(revision);
                            }
                            let outcome = if accepted {
                                ActionOutcome::Accepted
                            } else {
                                ActionOutcome::Failed
                            };
                            if request.reply.send(outcome).is_err() {
                                tracing::debug!(%target_id, "phone capacity refresh reply dropped after client disconnect");
                            }
                            tokio::task::yield_now().await;
                            continue;
                        }
                        ControllerAction::RefreshQuota { profile_id } => {
                            let known = controller.config.profiles.contains_key(profile_id);
                            if known {
                                // The refresher works from a generation-stamped
                                // batch, so a new generation is how one is asked
                                // for again rather than a per-profile trigger.
                                quota_batch.generation = quota_batch.generation.saturating_add(1);
                                quota_batch.profiles = quota_refresh_profiles(&controller);
                                quota_profiles_tx.send_replace(quota_batch.clone());
                            }
                            let outcome = if known {
                                ActionOutcome::Accepted
                            } else {
                                ActionOutcome::Failed
                            };
                            if request.reply.send(outcome).is_err() {
                                tracing::debug!(%profile_id, "phone quota refresh reply dropped after client disconnect");
                            }
                            tokio::task::yield_now().await;
                            continue;
                        }
                        _ => {}
                    }
                    if let ControllerAction::Cancel { session_id } = &request.action {
                        let outcome = if request_phone_action_cancellation(
                            session_id,
                            &action_sessions,
                            &action_cancellations,
                        ) {
                            daemon_runtime.cancel_lifecycle_if_active(session_id);
                            ActionOutcome::Accepted
                        } else {
                            ActionOutcome::NotCancellable
                        };
                        if request.reply.send(outcome).is_err() {
                            tracing::debug!(%session_id, "phone cancellation reply dropped after client disconnect");
                        }
                        tokio::task::yield_now().await;
                        continue;
                    }
                    let session_id = match admit_phone_action(
                        &request.action,
                        action_cancellations.len(),
                        &mut active_actions,
                    ) {
                        Ok(session_id) => session_id,
                        Err(refusal) => {
                            if request.reply.send(refusal).is_err() {
                                tracing::debug!("phone action refusal reply dropped after client disconnect");
                            }
                            tokio::task::yield_now().await;
                            continue;
                        }
                    };
                    let ControllerRequest { action, reply } = request;
                    let done = action_done_tx.clone();
                    let session_control = worker_commands_tx.clone();
                    let daemon_runtime = daemon_runtime.clone();
                    let started = action_started_tx.clone();
                    next_action_id = next_action_id.wrapping_add(1).max(1);
                    let action_id = next_action_id;
                    if let ControllerAction::New { workspace_id, .. } = &action {
                        let workspace_id = if workspace_id.is_empty() && phone_workspaces.len() == 1 {
                            phone_workspaces[0].id.clone()
                        } else {
                            workspace_id.clone()
                        };
                        launch_workspaces.insert(action_id, workspace_id);
                    }
                    let control = PhoneActionControl::for_action(&action);
                    action_cancellations.insert(action_id, control.clone());
                    if let Some(session_id) = &session_id {
                        action_sessions.insert(action_id, session_id.clone());
                    }
                    action_replies.accept(action_id, &action, reply);
                    let runtime = tokio::runtime::Handle::current();
                    tokio::spawn(async move {
                        let joined = tokio::task::spawn_blocking(move || {
                            let result = (|| -> Result<()> {
                                if control.cancelled.load(Ordering::Acquire) {
                                    bail!("phone action cancelled");
                                }
                                let mut operation_controller = Controller::load()?;
                                let executor =
                                    CancellableProcessExecutor::new(control.cancelled.clone());
                                runtime.block_on(apply_phone_action(
                                    &mut operation_controller,
                                    PhoneActionServices {
                                        sessions: &session_control,
                                        daemon_runtime: &daemon_runtime,
                                    },
                                    action,
                                    &executor,
                                    action_id,
                                    &started,
                                    &control,
                                ))
                            })();
                            result.map_err(|error| format!("{error:#}"))
                        })
                        .await;
                        let result = match joined {
                            Ok(result) => result,
                            Err(error) => Err(format!("phone action task failed: {error}")),
                        };
                        if let Err(error) = done.send((action_id, session_id, result)) {
                            tracing::debug!(action_id, %error, "phone action finished after the server stopped");
                        }
                    });
                }
                started = action_started_rx.recv() => {
                    let Some(started) = started else {
                        tokio::task::yield_now().await;
                        continue;
                    };
                    let publication = if !action_cancellations.contains_key(&started.action_id) {
                        Err("phone action completed before its provisional session was published".into())
                    } else {
                        track_started_phone_session(
                            &mut controller.state,
                            &mut active_actions,
                            &mut action_sessions,
                            started.action_id,
                            started.session,
                        )
                    };
                    if publication.is_ok() {
                        revision = daemon_runtime.allocate_revision();
                        if let Err(error) = snapshot_tx.send(viewer_snapshot(
                            &controller,
                            &phone_workspaces,
                            &quotas,
                            &PhoneSessionViews {
                                conversations: &conversations,
                                queued_prompts: &queued_prompts,
                                active_user_shells: &active_user_shells,
                                pending_elicitations: &pending_elicitations,
                                prompt_images: &prompt_images,
                                operational: &operational,
                                operations: &operations,
                                capacity: &viewer_capacity(&capacity_state),
                                launch_failures: &launch_failures,
                                reviews: &review_views(&daemon_runtime),
                            },
                            revision,
                        )) {
                            tracing::debug!(revision, %error, "phone snapshot delivery failed; no viewer is subscribed");
                        }
                        request_daemon_controller_reload(
                            daemon_runtime.clone(),
                            "new session publication",
                        );
                    };
                    if publication.is_err()
                        && let Some(control) = action_cancellations.get(&started.action_id)
                    {
                        control.request_cancel();
                    }
                    // The phone asked for a session, and now there is one to
                    // point at: that is what its request was waiting for.
                    action_replies.resolve(
                        started.action_id,
                        if publication.is_ok() {
                            ActionOutcome::Accepted
                        } else {
                            ActionOutcome::Failed
                        },
                    );
                    if started.published.send(publication).is_err() {
                        tracing::debug!(action_id = started.action_id, "phone new-session publication reply dropped after client disconnect");
                    }
                }
                completed = action_done_rx.recv() => {
                    let Some((action_id, session_id, result)) = completed else {
                        failure = feed_stopped(termination.is_cancelled(), "the phone action pipeline stopped reporting completions");
                        break;
                    };
                    action_cancellations.remove(&action_id);
                    let session_id = action_sessions.remove(&action_id).or(session_id);
                    if let Some(session_id) = &session_id {
                        active_actions.remove(session_id);
                    }
                    // A `new` that failed before publishing a session never
                    // reached the arm that answers it, so its phone is still
                    // waiting for a reply it can act on.
                    action_replies.resolve(action_id, ActionOutcome::Failed);
                    if let Some(workspace_id) = launch_workspaces.remove(&action_id)
                        && result.is_err()
                    {
                        record_launch_failure(&mut launch_failures, action_id, workspace_id);
                        revision = daemon_runtime.allocate_revision();
                        publish_snapshot!(revision);
                    }
                    if let Err(error) = &result {
                        tracing::warn!(action_id, %error, "phone action failed");
                    }
                    // Nothing is waiting on the request any more, so a failure
                    // the action itself did not record would reach no one but
                    // this process's stderr. Preserve it as an in-memory
                    // overlay for every later durable reload, where the
                    // snapshot's `has_error` takes it to the phone.
                    if let (Err(error), Some(session_id)) = (&result, &session_id)
                    {
                        pending_action_errors.insert(session_id.clone(), error.clone());
                    }
                    request_controller_reload(
                        &mut controller_reload_in_flight,
                        &mut controller_reload_requested,
                        &controller_reload_tx,
                    );
                    request_daemon_controller_reload(
                        daemon_runtime.clone(),
                        "phone action completion",
                    );
                }
                reloaded = controller_reload_rx.recv() => {
                    let Some(ControllerReloaded { result }) = reloaded else {
                        failure = feed_stopped(
                            termination.is_cancelled(),
                            "the controller reload pipeline stopped while the phone server was running",
                        );
                        break;
                    };
                    controller_reload_in_flight = false;
                    if std::mem::take(&mut controller_reload_invalidated) {
                        if let Err(error) = &result {
                            tracing::warn!(%error, "superseded controller reload failed");
                        }
                        controller_reload_requested = false;
                        request_controller_reload(
                            &mut controller_reload_in_flight,
                            &mut controller_reload_requested,
                            &controller_reload_tx,
                        );
                        continue;
                    }
                    match result {
                        Ok(mut reloaded) => {
                            for (session_id, error) in &pending_action_errors {
                                if let Some(session) = reloaded.state.sessions.get_mut(session_id)
                                    && session.last_error.is_none()
                                {
                                    session.last_error = Some(error.clone());
                                }
                            }
                            controller = reloaded;
                            worker_targets_tx.send_replace(dashboard_worker_targets(&controller));
                            publish_capacity_targets(
                                &controller,
                                &capacity_targets_tx,
                                &mut capacity_state,
                            );
                            credential_sync_handle.set_targets(credential_sync_targets(&controller));
                            republish_quota_profiles(
                                &controller,
                                &mut published_quota_profiles,
                                &mut quota_batch,
                                &quota_profiles_tx,
                            );
                            queued_prompts.retain(|session_id, _| {
                                controller.state.sessions.contains_key(session_id)
                            });
                            pending_elicitations.retain(|session_id, _| {
                                controller.state.sessions.contains_key(session_id)
                            });
                            prompt_images.retain(|session_id| {
                                controller.state.sessions.contains_key(session_id)
                            });
                            operational.retain(|session_id, _| {
                                controller.state.sessions.contains_key(session_id)
                            });
                            // A reload is the moment the controller's own view
                            // of what is running changes, so the operations the
                            // phone follows are re-read with it.
                            operations = daemon_runtime
                                .active_lifecycles()
                                .iter()
                                .map(|view| (view.session_id.clone(), viewer_operation(view)))
                                .collect();
                            conversations.retain(|id, _| {
                                controller.state.sessions.get(id).is_some_and(|session| session.state.is_active())
                            });
                            for session_id in conversation_projections.session_ids() {
                                if !controller
                                    .state
                                    .sessions
                                    .get(&session_id)
                                    .is_some_and(|session| session.state.is_active())
                                {
                                    conversation_projections.forget(&session_id);
                                }
                            }
                            revision = daemon_runtime.allocate_revision();
                            conversation_tx.send_replace(conversations.clone());
                            publish_snapshot!(revision);
                        }
                        Err(error) => {
                            tracing::warn!(%error, "completed phone operation could not reload controller state");
                        }
                    }
                    if controller_reload_requested {
                        controller_reload_requested = false;
                        controller_reload_in_flight = true;
                        spawn_controller_reload(controller_reload_tx.clone());
                    }
                }
            }
        }
        // Bundle jobs are supervised so shutdown never leaves a detached
        // request task behind holding the config mutation lock.
        bundle_jobs.shutdown().await;
        // Every exit stops in-flight work, whether it was asked for or forced.
        for control in action_cancellations.values() {
            control.request_cancel();
        }
        match failure {
            Some(failure) => Err(failure),
            None => Ok::<(), anyhow::Error>(()),
        }
    };
    let result = tokio::select! {
        result = serve => result,
        result = control => result,
    };
    conversation_projection_shutdown.cancel();
    renewal_cancellation.cancel();
    if let Some(task) = renewal_task
        && let Err(error) = task.await
    {
        tracing::warn!(%error, "Tailscale certificate renewal task failed");
    }
    worker_shutdown
        .shutdown()
        .await
        .context("shut down phone server session manager")?;
    result?;
    Ok(())
}

/// Why the control loop is stopping because one of its feeds ended.
///
/// During shutdown every feed ends, and that is the plan. At any other time it
/// means the phone server has lost the machinery it exists to drive, so it
/// says which feed and exits non-zero instead of reporting success.
fn feed_stopped(shutting_down: bool, reason: &'static str) -> Option<anyhow::Error> {
    (!shutting_down).then(|| anyhow::anyhow!(reason))
}

/// Whether a session's agent said it accepts image content in prompts. An
/// agent that has not answered `initialize` yet has advertised nothing, so the
/// phone is not offered controls the agent may refuse.
fn agent_accepts_prompt_images(operational: &hel::hel_worker::RelayOperationalState) -> bool {
    operational
        .agent_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.prompt_capabilities.image)
}

fn controller_action_session_id(action: &ControllerAction) -> Option<String> {
    match action {
        ControllerAction::New { .. } => None,
        ControllerAction::Prompt { session_id, .. }
        | ControllerAction::RunShell { session_id, .. }
        | ControllerAction::CancelShell { session_id, .. }
        | ControllerAction::Close { session_id }
        | ControllerAction::Resume { session_id, .. }
        | ControllerAction::Open { session_id }
        | ControllerAction::Cancel { session_id }
        | ControllerAction::RemoveQueuedPrompt { session_id, .. }
        | ControllerAction::RespondElicitation { session_id, .. }
        | ControllerAction::Rename { session_id, .. }
        | ControllerAction::CancelTurn { session_id }
        | ControllerAction::SetConfig { session_id, .. }
        | ControllerAction::SetPlanMode { session_id, .. }
        | ControllerAction::StartReview { session_id }
        | ControllerAction::ResolveReview { session_id, .. } => Some(session_id.clone()),
        // A refresh belongs to a profile or a target rather than a session, so
        // it takes no session slot and cannot be refused as session-busy.
        ControllerAction::RefreshQuota { .. } | ControllerAction::RefreshCapacity { .. } => None,
    }
}

/// Flatten a joined blocking result into the answer a phone channel carries.
fn flatten_stored<T>(
    joined: std::result::Result<Result<T>, tokio::task::JoinError>,
) -> std::result::Result<T, String> {
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(error) => Err(format!("viewer state task failed: {error}")),
    }
}

/// How one dirty repository is named to a phone.
///
/// The controller knows it by absolute path; a phone is told the leaf, which is
/// enough for a person to recognise the repository they are about to launch
/// over and says nothing about where it lives.
fn dirty_repository_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Perform the filesystem half of a new-session preflight.
///
/// This runs on the blocking task owned by the phone server. Bare targets
/// need the same directory-and-Git-HEAD validation as the dashboard; isolated
/// targets instead inspect the configured bundle for dirty local repositories.
fn run_new_preflight(
    config: HelConfig,
    bundle_id: String,
    target_id: String,
    project_directory: Option<PathBuf>,
) -> Result<mj_controller::hel_server::PreflightNew> {
    let target_is_bare = config
        .targets
        .get(&target_id)
        .with_context(|| format!("unknown target template {target_id:?}"))
        .map(is_bare_project_target)?;
    if target_is_bare {
        let directory =
            project_directory.context("project directory is required for a bare target")?;
        config_only_controller(config).validate_project_directory(
            &target_id,
            &directory,
            &ProcessExecutor,
        )?;
        return Ok(mj_controller::hel_server::PreflightNew {
            dirty_repositories: Vec::new(),
        });
    }
    if project_directory.is_some() {
        bail!("project directory is unsupported for this target");
    }

    let bundle = config.bundles.get(&bundle_id).context("unknown bundle")?;
    let dirty = hel::hel_local_git::dirty_local_repositories(bundle)?
        .into_iter()
        .map(|repository| dirty_repository_label(&repository.path))
        .collect();
    Ok(mj_controller::hel_server::PreflightNew {
        dirty_repositories: dirty,
    })
}

fn phone_action_capacity_available(active_actions: usize) -> bool {
    active_actions < MAX_CONCURRENT_PHONE_ACTIONS
}

/// Point the quota refresher at the profiles the configuration currently
/// defines, alongside the worker-poll and credential-sync targets that are
/// rebuilt from the same reload. A profile added to `config.toml` while the
/// server runs otherwise reaches the snapshot but never the refresher, and
/// reads "quota unavailable" until the next restart.
///
/// Sending a batch restarts every profile's refresh, which spawns a harness
/// process per profile, so the batch travels only when the profiles changed.
/// Reports whether it did.
fn republish_quota_profiles(
    controller: &Controller,
    published: &mut std::collections::BTreeMap<String, HarnessProfile>,
    batch: &mut QuotaRefreshBatch,
    profiles_tx: &tokio::sync::watch::Sender<QuotaRefreshBatch>,
) -> bool {
    if *published == controller.config.profiles {
        return false;
    }
    published.clone_from(&controller.config.profiles);
    batch.generation = batch.generation.saturating_add(1);
    batch.profiles = quota_refresh_profiles(controller);
    profiles_tx.send_replace(batch.clone());
    true
}

fn request_phone_action_cancellation(
    session_id: &str,
    action_sessions: &std::collections::BTreeMap<u64, String>,
    action_cancellations: &std::collections::BTreeMap<u64, PhoneActionControl>,
) -> bool {
    let control = action_sessions
        .iter()
        .find_map(|(action_id, active_session_id)| {
            (active_session_id == session_id)
                .then(|| action_cancellations.get(action_id))
                .flatten()
        });
    if let Some(control) = control {
        return control.request_cancel();
    }
    false
}

fn track_started_phone_session(
    state: &mut HelState,
    active_actions: &mut std::collections::BTreeSet<String>,
    action_sessions: &mut std::collections::BTreeMap<u64, String>,
    action_id: u64,
    session: SessionRecord,
) -> std::result::Result<(), String> {
    let session_id = session.id.clone();
    if !active_actions.insert(session_id.clone()) {
        return Err("another operation is already running for the new session".into());
    }
    action_sessions.insert(action_id, session_id.clone());
    state.sessions.insert(session_id, session);
    Ok(())
}

/// Retain safe notices even if provisioning removed its provisional session.
/// This bounded, daemon-lifetime history survives durable controller reloads.
fn record_launch_failure(
    failures: &mut Vec<mj_controller::hel_server::ViewerLaunchFailure>,
    action_id: u64,
    workspace_id: String,
) {
    failures.push(mj_controller::hel_server::ViewerLaunchFailure {
        id: format!("{}-{action_id}", std::process::id()),
        workspace_id,
    });
    if failures.len() > 16 {
        failures.remove(0);
    }
}

struct PhoneActionServices<'a> {
    sessions: &'a SessionManagerControl,
    daemon_runtime: &'a Arc<RuntimeState>,
}

async fn apply_phone_action(
    controller: &mut Controller,
    services: PhoneActionServices<'_>,
    action: ControllerAction,
    executor: &(impl CommandExecutor + Sync),
    action_id: u64,
    started: &tokio::sync::mpsc::UnboundedSender<PhoneActionStarted>,
    control: &PhoneActionControl,
) -> Result<()> {
    match action {
        ControllerAction::New {
            workspace_id,
            profile_id,
            bundle_id,
            target_id,
            title,
            project_directory,
            dirty_ack,
        } => {
            let workspace_id = if workspace_id.is_empty() {
                let workspaces = hel::hel_database::list_workspaces()?;
                match workspaces.as_slice() {
                    [workspace] => workspace.id.clone(),
                    [] => bail!("create a workspace before starting a phone session"),
                    _ => bail!("phone session creation requires a workspace_id"),
                }
            } else {
                workspace_id
            };
            // A phone that supplies no title gets the one the terminal would
            // have derived, so a session started from either surface reads the
            // same way in both.
            let title = title.unwrap_or_else(|| {
                let project = project_directory
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| bundle_id.clone());
                format!("{project} via {profile_id}")
            });
            let session_title_override = Some(title.clone());
            // The acknowledgement has to match the repositories that are dirty
            // now, not the ones that were dirty when the phone was asked. A
            // launch over changes nobody saw is the thing this prevents.
            let allow_dirty_local = if dirty_ack.is_empty() {
                false
            } else {
                let controller_bundle = controller
                    .config
                    .bundles
                    .get(&bundle_id)
                    .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
                let dirty = hel::hel_local_git::dirty_local_repositories(controller_bundle)?
                    .into_iter()
                    .map(|repository| dirty_repository_label(&repository.path))
                    .collect::<std::collections::BTreeSet<_>>();
                let acknowledged = dirty_ack.iter().cloned().collect();
                if dirty != acknowledged {
                    bail!(
                        "the repositories with uncommitted changes are not the ones that were acknowledged; check again"
                    );
                }
                true
            };
            let session_id = controller.register_session_with_resources(
                &profile_id,
                &bundle_id,
                &target_id,
                title,
                SessionLaunchOptions {
                    workspace_id,
                    additional_mounts: Vec::new(),
                    allow_dirty_local,
                    resource_allocation: None,
                    project_directory,
                    session_title_override,
                },
            )?;
            let session = controller
                .state
                .sessions
                .get(&session_id)
                .expect("newly registered phone session exists")
                .clone();
            let (published, publication) = tokio::sync::oneshot::channel();
            let publish_result = started
                .send(PhoneActionStarted {
                    action_id,
                    session,
                    published,
                })
                .map_err(|_| anyhow::anyhow!("phone server stopped before publishing session"));
            let publish_result = match publish_result {
                Ok(()) => publication
                    .await
                    .map_err(|_| anyhow::anyhow!("phone server stopped before publishing session"))?
                    .map_err(anyhow::Error::msg),
                Err(error) => Err(error),
            };
            if let Err(error) = publish_result {
                control.request_cancel();
                let rollback = controller
                    .provision_session_controlled(&session_id, executor)
                    .await;
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(error.context(format!(
                        "discard provisional session after publication failure: {rollback:#}"
                    ))),
                };
            }
            controller
                .provision_session_controlled_with_commit(&session_id, executor, || {
                    if control.grant_new_commit() {
                        Ok(())
                    } else {
                        bail!("phone action cancelled before session commit")
                    }
                })
                .await
        }
        ControllerAction::Prompt {
            session_id,
            text,
            images,
        } => {
            services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?
                .submit(
                    new_command_id("phone-prompt")?,
                    RelayCommand::Prompt {
                        prompt: phone_prompt_blocks(text, images),
                    },
                )
                .await?;
            Ok(())
        }
        ControllerAction::RunShell {
            session_id,
            command,
        } => {
            services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?
                .submit(
                    new_command_id("phone-shell")?,
                    RelayCommand::RunUserShell { command },
                )
                .await?;
            Ok(())
        }
        ControllerAction::CancelShell {
            session_id,
            shell_command_id,
        } => {
            services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?
                .submit(
                    new_command_id("phone-cancel-shell")?,
                    RelayCommand::CancelUserShell { shell_command_id },
                )
                .await?;
            Ok(())
        }
        ControllerAction::Close { session_id } => {
            services.daemon_runtime.close_session(session_id).await
        }
        ControllerAction::Resume {
            session_id,
            workspace_id,
            profile_id,
            target_id,
            queue,
        } => services
            .daemon_runtime
            .resume_session(ResumeSessionRequest {
                session_id,
                workspace_id,
                profile_id,
                target_template_id: target_id,
                additional_mounts: None,
                resource_allocation: None,
                discard_queue: queue == ResumeQueueDisposition::Discard,
                repository_preflight: None,
            })
            .await
            .map(|_| ()),
        ControllerAction::Open { .. } => Ok(()),
        ControllerAction::Cancel { .. } => {
            bail!("cancel actions must be handled by the phone control loop")
        }
        ControllerAction::RemoveQueuedPrompt {
            session_id,
            queue_id,
        } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .submit(
                    new_command_id("phone-remove-prompt")?,
                    RelayCommand::RemoveQueuedPrompt {
                        queued_command_id: queue_id,
                    },
                )
                .await?;
            Ok(())
        }
        ControllerAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .respond_elicitation(elicitation_id, response)
                .await
        }
        ControllerAction::Rename { session_id, title } => {
            controller.rename_session(&session_id, &title)?;
            Ok(())
        }
        ControllerAction::StartReview { session_id } => {
            // The refusal is a sentence for the person holding the phone --
            // "prompts are queued", "set [review] profile in config.toml" --
            // so it travels as the error text of this action.
            services
                .daemon_runtime
                .review_host()
                .start(&session_id, true)
                .await
                .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
            Ok(())
        }
        ControllerAction::ResolveReview {
            session_id,
            resolution,
        } => {
            let resolution = mj_controller::hel_server::resolution_from_name(&resolution)
                .context("a review is resolved by forward, dismiss, or cancel")?;
            services
                .daemon_runtime
                .review_host()
                .resolve(&session_id, resolution)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(())
        }
        ControllerAction::CancelTurn { session_id } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .submit(new_command_id("phone-cancel-turn")?, RelayCommand::Cancel)
                .await?;
            Ok(())
        }
        ControllerAction::SetConfig {
            session_id,
            key,
            value,
        } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .submit(
                    new_command_id("phone-set-config")?,
                    RelayCommand::SetConfig { key, value },
                )
                .await?;
            Ok(())
        }
        ControllerAction::SetPlanMode { session_id, active } => {
            // Which call turns plan mode on is a fact about the harness, so it
            // is asked of the shared decision rather than decided here or, far
            // worse, in the browser.
            let harness_kind = controller
                .state
                .sessions
                .get(&session_id)
                .with_context(|| format!("unknown session {session_id}"))?
                .harness_kind;
            let handle = services.sessions.session(&session_id).await?;
            let operational = handle
                .view()
                .snapshot
                .map(|snapshot| snapshot.operational)
                .context("the session has not reported what it supports yet")?;
            let facts = hel::hel_acp::AcpSessionFacts::from_operational(
                harness_kind,
                &operational.config,
                &operational.config_options,
                operational.modes.as_ref(),
            );
            let command = match facts.plan_control(active) {
                Ok(hel::hel_acp::PlanControl::SetConfig { key, value }) => {
                    RelayCommand::SetConfig { key, value }
                }
                Ok(hel::hel_acp::PlanControl::SetSessionMode { mode_id }) => {
                    RelayCommand::SetSessionMode { mode_id }
                }
                Err(reason) => bail!("{reason}"),
            };
            handle
                .submit(new_command_id("phone-plan-mode")?, command)
                .await?;
            Ok(())
        }
        // Refreshes are handled by the phone control loop, which owns the
        // pollers they nudge.
        ControllerAction::RefreshQuota { .. } | ControllerAction::RefreshCapacity { .. } => {
            bail!("refresh actions must be handled by the phone control loop")
        }
    }
}

/// The live, per-session projections the phone snapshot layers on top of the
/// controller's durable state. They arrive from relay snapshots rather than
/// from disk, so they travel together instead of as separate arguments.
struct PhoneSessionViews<'a> {
    conversations:
        &'a std::collections::BTreeMap<String, mj_controller::hel_server::BrowserTranscript>,
    queued_prompts: &'a std::collections::BTreeMap<String, Vec<hel::hel_worker::QueuedPrompt>>,
    active_user_shells:
        &'a std::collections::BTreeMap<String, Vec<hel::hel_worker::ActiveUserShell>>,
    pending_elicitations:
        &'a std::collections::BTreeMap<String, Vec<hel::hel_elicitation::ElicitationRequest>>,
    /// Sessions whose agent advertised image support in prompts.
    prompt_images: &'a std::collections::BTreeSet<String>,
    /// What each managed session's relay last reported. This is where the
    /// projection learns what the agent can do, rather than guessing from the
    /// durable record, which knows only what was configured.
    operational: &'a std::collections::BTreeMap<String, hel::hel_worker::RelayOperationalState>,
    /// Lifecycle operations running now, keyed by session.
    operations: &'a std::collections::BTreeMap<String, mj_controller::hel_server::ViewerOperation>,
    /// The most recent capacity reading per probe target.
    capacity: &'a [mj_controller::hel_server::ViewerTargetCapacity],
    launch_failures: &'a [mj_controller::hel_server::ViewerLaunchFailure],
    /// Reviews the daemon is running, keyed by session. The phone renders the
    /// same review the terminal does, from the same host.
    reviews:
        &'a std::collections::BTreeMap<String, mj_controller::hel_review_host::RuntimeReviewView>,
}

/// What the phone server remembers about one probe target between readings.
///
/// The last good reading is kept beside any failure, because one failed probe
/// is not a reason to forget what a machine was doing a minute ago; the phone
/// is told both, and says so.
#[derive(Debug, Clone)]
struct PhoneCapacity {
    target: hel::hel_targets::DeploymentCapacityTarget,
    usage: Option<hel::hel_targets::DeploymentCapacityUsage>,
    on_demand: bool,
    sampled_at_epoch_seconds: Option<u64>,
    refreshing: bool,
    failed: bool,
}

/// How old a reading may be before the page says so.
const CAPACITY_STALE_AFTER: Duration = Duration::from_secs(120);

/// Tell the poller which targets to probe, and keep the state map in step.
fn publish_capacity_targets(
    controller: &Controller,
    targets_tx: &tokio::sync::watch::Sender<Vec<hel::hel_targets::DeploymentCapacityTarget>>,
    state: &mut std::collections::BTreeMap<String, PhoneCapacity>,
) {
    let targets = controller.deployment_capacity_targets();
    state.retain(|id, _| targets.iter().any(|target| target.id == *id));
    for target in &targets {
        state
            .entry(target.id.clone())
            .and_modify(|entry| entry.target = target.clone())
            .or_insert_with(|| PhoneCapacity {
                target: target.clone(),
                usage: None,
                on_demand: false,
                sampled_at_epoch_seconds: None,
                // A target with no reading yet is loading, not idle.
                refreshing: true,
                failed: false,
            });
    }
    if targets_tx.borrow().as_slice() != targets.as_slice() {
        targets_tx.send_replace(targets);
    }
}

/// Project the capacity readings for the phone.
fn viewer_capacity(
    state: &std::collections::BTreeMap<String, PhoneCapacity>,
) -> Vec<mj_controller::hel_server::ViewerTargetCapacity> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    state
        .values()
        .map(|entry| {
            let usage = entry.usage.as_ref();
            mj_controller::hel_server::ViewerTargetCapacity {
                id: entry.target.id.clone(),
                label: entry.target.host.clone(),
                target_ids: entry.target.target_ids.clone(),
                cpu_percent: usage.and_then(|usage| usage.cpu_percent),
                memory_used_bytes: usage.map(|usage| usage.memory_used_bytes),
                memory_total_bytes: usage.map(|usage| usage.memory_total_bytes),
                logical_cores: usage.map(|usage| usage.logical_cores),
                disk_total_bytes: usage.and_then(|usage| usage.disk_total_bytes),
                // A fleet reports how many machines it is running; a plain host
                // has no such count and says nothing rather than zero.
                virtual_machines: matches!(
                    entry.target.kind,
                    hel::hel_targets::DeploymentCapacityKind::AwsFleet
                )
                .then(|| u64::from(!entry.on_demand)),
                sampled_at_epoch_seconds: entry.sampled_at_epoch_seconds,
                refreshing: entry.refreshing,
                stale: entry.sampled_at_epoch_seconds.is_some_and(|sampled| {
                    now.saturating_sub(sampled) > CAPACITY_STALE_AFTER.as_secs()
                }),
                has_error: entry.failed,
            }
        })
        .collect()
}

/// Turn one lifecycle operation into the projection a phone follows.
fn viewer_operation(
    view: &crate::daemon::RuntimeLifecycleView,
) -> mj_controller::hel_server::ViewerOperation {
    use mj_controller::hel_server::{ViewerOperationKind, ViewerOperationStage};

    mj_controller::hel_server::ViewerOperation {
        // The session owns at most one operation at a time, so its id is a
        // stable name for the operation without inventing a second counter.
        id: view.session_id.clone(),
        session_id: view.session_id.clone(),
        kind: match view.kind {
            crate::daemon::RuntimeLifecycleKind::Create => ViewerOperationKind::Create,
            crate::daemon::RuntimeLifecycleKind::Resume => ViewerOperationKind::Resume,
            // A force stop and a destroy are both stops as far as a phone is
            // concerned: it watches one thing end, and the difference is in
            // how much the controller tears down behind it.
            crate::daemon::RuntimeLifecycleKind::Close
            | crate::daemon::RuntimeLifecycleKind::ForceStop
            | crate::daemon::RuntimeLifecycleKind::DestroyStopped
            | crate::daemon::RuntimeLifecycleKind::ForceDestroy
            | crate::daemon::RuntimeLifecycleKind::Cleanup => ViewerOperationKind::Stop,
        },
        started_at_epoch_seconds: view.started_at_epoch_seconds,
        stages: view
            .active_stages
            .iter()
            .map(|(stage, started_at)| ViewerOperationStage {
                label: stage.label(),
                started_at_epoch_seconds: *started_at,
            })
            .collect(),
        notice: view.notice.clone(),
        cancellable: true,
    }
}

/// What the phone may do with one session.
///
/// Everything here is a fact the controller holds and the browser cannot:
/// whether the session manager is driving this session, what the agent said it
/// supports, and whether a lifecycle operation already owns it.
fn session_capabilities(
    session: &mj_controller::hel_server::ViewerSession,
    operational: Option<&hel::hel_worker::RelayOperationalState>,
    operation: Option<&mj_controller::hel_server::ViewerOperation>,
    facts: Option<&hel::hel_acp::AcpSessionFacts>,
) -> mj_controller::hel_server::ViewerSessionCapabilities {
    use mj_controller::hel_server::ViewerLifecycleCategory;

    let live = session.lifecycle == ViewerLifecycleCategory::Live;
    // A session the manager is not driving cannot be talked to, whatever its
    // durable state says.
    let attached = operational.is_some();
    let busy = operation.is_some();
    let idle = operational
        .is_some_and(|state| state.execution == hel::hel_worker::RelayExecutionState::Idle);
    mj_controller::hel_server::ViewerSessionCapabilities {
        open: session.conversation_available,
        prompt: live && attached,
        run_shell: live && attached,
        cancel_turn: live && operational.is_some_and(|state| state.active_prompt.is_some()),
        cancel_operation: busy,
        // Stopping a session that is already stopping asks for something that
        // is happening; resuming one that is running asks for a second copy.
        stop: session.lifecycle.is_dashboard_visible() && !busy,
        rename: true,
        resume: !session.lifecycle.is_dashboard_visible() && !busy,
        set_config: live && attached && facts.is_some(),
        // Plan mode is a turn boundary: the terminal offers it only while the
        // agent is idle, and the phone must not be looser.
        set_plan_mode: live
            && idle
            && facts.is_some_and(hel::hel_acp::AcpSessionFacts::supports_plan_mode),
    }
}

/// The settings this agent advertised, with the values it accepts.
fn viewer_config_options(
    operational: &hel::hel_worker::RelayOperationalState,
) -> Vec<mj_controller::hel_server::ViewerConfigOption> {
    use mj_controller::hel_server::{ViewerConfigChoice, ViewerConfigOption};

    ["model", "effort"]
        .into_iter()
        .filter_map(|key| {
            let choices = hel::hel_acp::session_config_choices(&operational.config_options, key);
            if choices.is_empty() {
                return None;
            }
            Some(ViewerConfigOption {
                key: key.to_owned(),
                label: key.to_owned(),
                current: operational.config.get(key).cloned(),
                choices: choices
                    .into_iter()
                    .map(|choice| ViewerConfigChoice {
                        value: choice.value,
                        name: choice.name,
                        description: choice.description,
                    })
                    .collect(),
            })
        })
        .collect()
}

/// The ACP content blocks one phone prompt becomes: its text, then each
/// attached image as the image block the prompt path already carries.
fn phone_prompt_blocks(
    text: String,
    images: Vec<mj_controller::hel_server::ViewerPromptImage>,
) -> Vec<agent_client_protocol::schema::v1::ContentBlock> {
    use agent_client_protocol::schema::v1::{ContentBlock, ImageContent, TextContent};

    let mut prompt = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        prompt.push(ContentBlock::Text(TextContent::new(text)));
    }
    prompt.extend(
        images.into_iter().map(|image| {
            ContentBlock::Image(ImageContent::new(image.data_base64, image.mime_type))
        }),
    );
    prompt
}

/// The Mjolnir commands a phone may offer for one session.
///
/// The list is built here, from what this session can actually do, and
/// published: the browser used to keep its own copy, which is how `/review`
/// was missing from the phone while the terminal had it.
fn phone_commands(
    session: &mj_controller::hel_server::ViewerSession,
    operational: Option<&hel::hel_worker::RelayOperationalState>,
) -> Vec<mj_controller::hel_server::ViewerMjCommand> {
    use agent_client_protocol::schema::v1::AvailableCommandInput;
    use mj_controller::hel_server::ViewerCommandSource;

    let command = |name: &str, description: &str, argument: Option<&str>| {
        mj_controller::hel_server::ViewerMjCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            source: ViewerCommandSource::Mj,
            argument: argument.map(str::to_owned),
        }
    };
    let mut commands = vec![
        command("help", "show available Mjolnir and agent commands", None),
        command(
            "detach",
            "leave the conversation without stopping the worker",
            None,
        ),
    ];
    let option = |key: &str| {
        session
            .config_options
            .iter()
            .any(|option| option.key == key)
    };
    if option("model") {
        commands.push(command("model", "change the active model", Some("value")));
        commands.push(command("fast", "toggle Codex Fast mode", None));
    }
    if option("effort") {
        commands.push(command(
            "effort",
            "change the active reasoning effort",
            Some("value"),
        ));
    }
    if session.plan_mode_active.is_some() && session.capabilities.set_plan_mode {
        commands.push(command("plan", "toggle plan mode", Some("message")));
        commands.push(command(
            "implement",
            "leave plan mode and implement",
            Some("instruction"),
        ));
    }
    if session.capabilities.prompt || session.turn_review.is_some() {
        commands.push(command(
            "review",
            "review the finished turn now, or report how review is configured",
            Some("status"),
        ));
    }
    // These names are handled by Mjolnir even when the corresponding control
    // is unavailable for this session. An agent cannot claim one and turn a
    // locally interpreted slash command into a misleading palette entry.
    let reserved = [
        "help",
        "detach",
        "model",
        "fast",
        "effort",
        "plan",
        "implement",
        "review",
    ];
    for advertised in operational
        .into_iter()
        .flat_map(|state| state.available_commands.iter())
    {
        let name = advertised.name.trim();
        if name.is_empty()
            || reserved
                .iter()
                .any(|local| name.eq_ignore_ascii_case(local))
            || commands
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let argument = advertised.input.as_ref().and_then(|input| match input {
            AvailableCommandInput::Unstructured(input) => {
                let hint = input.hint.trim();
                (!hint.is_empty()).then(|| hint.to_owned())
            }
            _ => None,
        });
        commands.push(mj_controller::hel_server::ViewerMjCommand {
            name: name.to_owned(),
            description: advertised.description.trim().to_owned(),
            source: ViewerCommandSource::Agent,
            argument,
        });
    }
    commands
}

/// The open reviews, keyed by session, for one snapshot.
fn review_views(
    daemon_runtime: &Arc<RuntimeState>,
) -> std::collections::BTreeMap<String, mj_controller::hel_review_host::RuntimeReviewView> {
    daemon_runtime
        .review_host()
        .views()
        .into_iter()
        .map(|review| (review.session_id.clone(), review))
        .collect()
}

fn viewer_snapshot(
    controller: &Controller,
    workspaces: &[hel::hel_workspace::WorkspaceRecord],
    quotas: &std::collections::BTreeMap<String, ProfileQuota>,
    views: &PhoneSessionViews<'_>,
    revision: u64,
) -> ViewerSnapshot {
    let PhoneSessionViews {
        conversations,
        reviews,
        queued_prompts,
        active_user_shells,
        pending_elicitations,
        prompt_images,
        operational,
        operations,
        capacity,
        launch_failures,
    } = views;
    let mut snapshot =
        ViewerSnapshot::from_config_state(&controller.config, &controller.state, revision);
    snapshot.launch_failures = launch_failures.to_vec();
    snapshot.workspaces = workspaces
        .iter()
        .map(|workspace| mj_controller::hel_server::ViewerWorkspace {
            id: workspace.id.clone(),
            name: workspace.name.clone(),
        })
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for profile in &mut snapshot.profiles {
        let Some(quota) = quotas.get(&profile.id) else {
            continue;
        };
        profile.quota = Some(ViewerQuota {
            summary: quota.compact(),
            windows: quota
                .windows
                .iter()
                .map(|window| mj_controller::hel_server::ViewerQuotaWindow {
                    label: window.label.clone(),
                    // The controller reports headroom; a bar fills as a limit
                    // is consumed, so the phone is given the complement.
                    percent_used: window
                        .remaining_percent
                        .map(|left| 100_u8.saturating_sub(left)),
                    resets_at: window.resets.clone(),
                    projects_exhaustion_before_reset: mj_controller::hel_quota::projects_exhaustion(
                        window,
                        quota.refreshed_at_epoch_seconds,
                    ),
                })
                .collect(),
            resets_at: quota
                .windows
                .iter()
                .find_map(|window| window.resets.clone()),
            stale: now.saturating_sub(quota.refreshed_at_epoch_seconds)
                > QUOTA_STALE_AFTER.as_secs(),
            refreshed_at_epoch_seconds: quota.refreshed_at_epoch_seconds,
            has_error: quota.error.is_some(),
        });
    }
    for session in &mut snapshot.sessions {
        session.queued_prompts = queued_prompts
            .get(&session.id)
            .into_iter()
            .flatten()
            .map(|prompt| ViewerQueuedPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
                created_at: prompt.created_at_ms.to_string(),
            })
            .collect();
        session.active_user_shells = active_user_shells
            .get(&session.id)
            .into_iter()
            .flatten()
            .map(|shell| ViewerUserShell {
                id: shell.command_id.clone(),
                command: shell.command.clone(),
                started_at_ms: shell.started_at_ms,
            })
            .collect();
        session.pending_elicitations = pending_elicitations
            .get(&session.id)
            .cloned()
            .unwrap_or_default();
        session.prompt_images_supported = prompt_images.contains(&session.id);
        session.operation = operations.get(&session.id).cloned();
        let live = operational.get(&session.id);
        let facts = live.map(|state| {
            hel::hel_acp::AcpSessionFacts::from_operational(
                controller
                    .state
                    .sessions
                    .get(&session.id)
                    .map_or(hel::hel_config::HarnessKind::Codex, |record| {
                        record.harness_kind
                    }),
                &state.config,
                &state.config_options,
                state.modes.as_ref(),
            )
        });
        if let Some(state) = live {
            session.latest_event_ordinal = state.latest_ordinal;
            session.chat_phase = match state.execution {
                hel::hel_worker::RelayExecutionState::Idle => {
                    mj_controller::hel_server::ViewerChatPhase::Idle
                }
                hel::hel_worker::RelayExecutionState::Running => {
                    mj_controller::hel_server::ViewerChatPhase::Running
                }
                hel::hel_worker::RelayExecutionState::Closing => {
                    mj_controller::hel_server::ViewerChatPhase::Closing
                }
                hel::hel_worker::RelayExecutionState::Closed => {
                    mj_controller::hel_server::ViewerChatPhase::Closed
                }
            };
            session.config_options = viewer_config_options(state);
            // The same three states the terminal's expanded row shows, from
            // the same helper, so the phone never disagrees with it.
            let turn_started_at = state
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or_else(|| state.harness_turn.map(|turn| turn.started_at_ms))
                .and_then(|started_at_ms| u64::try_from(started_at_ms / 1_000).ok());
            let activity = mj_chat::usage_format::SessionActivity::of(state);
            session.is_idle = controller
                .state
                .sessions
                .get(&session.id)
                .is_some_and(|record| record.state == hel::hel_state::SessionState::Running)
                && session.operation.is_none()
                && activity.is_idle(turn_started_at);
            session.activity = mj_chat::usage_format::format_activity_columns(
                now,
                turn_started_at,
                state
                    .current_step_started_at_ms
                    .and_then(|value| u64::try_from(value).ok()),
                &activity,
            )
            .join("  ")
            .trim()
            .to_owned();
        }
        session.plan_mode_active = facts
            .as_ref()
            .filter(|facts| facts.supports_plan_mode())
            .map(hel::hel_acp::AcpSessionFacts::plan_mode_active);
        session.turn_review = reviews
            .get(&session.id)
            .map(mj_controller::hel_server::ViewerTurnReview::from_runtime);
        session.capabilities =
            session_capabilities(session, live, operations.get(&session.id), facts.as_ref());
        session.available_commands = phone_commands(session, live);
        if let Some(transcript) = conversations.get(&session.id) {
            session.conversation_available = true;
            let mut lines = transcript
                .entries
                .iter()
                .flat_map(|entry| {
                    entry
                        .lines
                        .iter()
                        .enumerate()
                        .filter_map(move |(index, line)| {
                            let line = line.trim();
                            (!line.is_empty()).then(|| {
                                if index == 0 {
                                    format!("{}: {line}", entry.label)
                                } else {
                                    line.to_owned()
                                }
                            })
                        })
                })
                .collect::<Vec<_>>();
            session.preview = lines.split_off(lines.len().saturating_sub(4));
        }
        // `conversation_available` is only known after the transcript loop
        // above, so the capability that depends on it is settled here.
        session.capabilities.open = session.conversation_available;
    }
    snapshot.capacity = capacity.to_vec();
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pollers::QUOTA_REFRESH_INTERVAL;
    use hel::hel_config::{
        CONFIG_VERSION, HarnessKind, HelConfig, ProjectBundle, ProjectRepository, TargetTemplate,
    };
    use hel::hel_state::SessionState;

    #[tokio::test]
    async fn explicit_tls_takes_precedence_over_tailscale_detection() {
        let resolved = resolve_server_args(
            ServerArgs {
                bind: "0.0.0.0:4443".into(),
                tailscale_detect: true,
                tls_cert: Some(PathBuf::from("configured-cert.pem")),
                tls_key: Some(PathBuf::from("configured-key.pem")),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(resolved.bind, "0.0.0.0:4443".parse().unwrap());
        assert_eq!(resolved.viewer_url, "https://0.0.0.0:4443");
        assert_eq!(
            resolved.tls_files,
            Some((
                PathBuf::from("configured-cert.pem"),
                PathBuf::from("configured-key.pem")
            ))
        );
        assert!(resolved.tailscale.is_none());
        assert!(resolved.fallback_reason.is_none());
    }

    #[tokio::test]
    async fn disabling_detection_keeps_the_viewer_on_loopback() {
        let resolved = resolve_server_args(
            ServerArgs {
                bind: "127.0.0.1:4765".into(),
                tailscale_detect: false,
                tls_cert: None,
                tls_key: None,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(resolved.bind, "127.0.0.1:4765".parse().unwrap());
        assert_eq!(resolved.viewer_url, "http://127.0.0.1:4765");
        assert!(
            resolved
                .fallback_reason
                .unwrap()
                .contains("detection is disabled")
        );
    }

    fn bare_preflight_config() -> HelConfig {
        let mut config = HelConfig::default();
        config
            .targets
            .insert("raw".into(), TargetTemplate::LocalBare);
        config
    }

    #[test]
    fn new_preflight_rejects_a_bare_project_without_a_git_head() {
        let error = run_new_preflight(
            bare_preflight_config(),
            "hel".into(),
            "raw".into(),
            Some(PathBuf::from("/definitely/not/a/project")),
        )
        .expect_err("a missing project directory must fail preflight");

        assert!(
            error
                .to_string()
                .contains("project directory does not exist or is not a directory")
        );
    }

    #[test]
    fn new_preflight_accepts_a_git_project_for_a_bare_target() {
        let directory = std::env::current_dir().expect("the test has a working directory");
        let answer = run_new_preflight(
            bare_preflight_config(),
            "hel".into(),
            "raw".into(),
            Some(directory),
        )
        .expect("the repository running the test has a valid Git HEAD");

        assert!(answer.dirty_repositories.is_empty());
    }

    #[test]
    fn new_preflight_keeps_bundle_dirty_check_for_isolated_targets() {
        let mut config = HelConfig::default();
        config.targets.insert(
            "podman".into(),
            TargetTemplate::LocalPodman {
                container: hel::hel_config::ContainerTemplate {
                    image: "test-image".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: Default::default(),
                    workspace_storage: Default::default(),
                },
            },
        );
        config.bundles.insert(
            "hel".into(),
            ProjectBundle {
                primary_repo: "hel".into(),
                repositories: vec![ProjectRepository {
                    id: "hel".into(),
                    github: Some("owner/hel".into()),
                    local: None,
                    destination: "hel".into(),
                    git_ref: None,
                }],
            },
        );
        let answer = run_new_preflight(config, "hel".into(), "podman".into(), None)
            .expect("a GitHub-only bundle has no local dirty repositories");

        assert!(answer.dirty_repositories.is_empty());
    }

    #[test]
    fn a_phone_prompt_becomes_its_text_then_its_images() {
        use agent_client_protocol::schema::v1::ContentBlock;

        let image = |data: &str| mj_controller::hel_server::ViewerPromptImage {
            data_base64: data.into(),
            mime_type: "image/png".into(),
            width: 32,
            height: 24,
        };
        let blocks = phone_prompt_blocks(
            "look at this".into(),
            vec![image("aW1hZ2U="), image("c2Vjb25k")],
        );
        let ContentBlock::Text(text) = &blocks[0] else {
            panic!("the prompt leads with its text");
        };
        assert_eq!(text.text, "look at this");
        let ContentBlock::Image(first) = &blocks[1] else {
            panic!("each attachment travels as an image block");
        };
        assert_eq!(first.data, "aW1hZ2U=");
        assert_eq!(first.mime_type, "image/png");
        assert!(matches!(blocks[2], ContentBlock::Image(_)));
        assert_eq!(blocks.len(), 3);

        // An image needs no words with it, and an empty text block would be a
        // message the user never wrote.
        let images_only = phone_prompt_blocks(String::new(), vec![image("aW1hZ2U=")]);
        assert_eq!(images_only.len(), 1);
        assert!(matches!(images_only[0], ContentBlock::Image(_)));
    }

    #[test]
    fn image_prompts_are_offered_only_after_the_agent_advertises_them() {
        use agent_client_protocol::schema::v1::AgentCapabilities;
        use hel::hel_worker::{RelayExecutionState, RelayOperationalState};

        let operational = |agent_capabilities| RelayOperationalState {
            session_id: "session-1".into(),
            execution: RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: String::new(),
            acknowledged_through: 0,
            acknowledged_digest: String::new(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: String::new(),
            native_session_id: None,
            agent_capabilities,
            agent_info: None,
            config_options: Vec::new(),
            modes: None,
            available_commands: Vec::new(),
            config: std::collections::BTreeMap::new(),
            active_prompt: None,
            queued_prompts: Vec::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            checkpoint_barrier: None,
            checkpoint_ready: None,
            last_acp_activity_at_ms: None,
            current_step_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            background_commands: Vec::new(),
        };

        // A session whose agent has not answered `initialize` has advertised
        // nothing, so the phone is not offered a control the agent may refuse.
        assert!(!agent_accepts_prompt_images(&operational(None)));
        assert!(!agent_accepts_prompt_images(&operational(Some(Box::new(
            AgentCapabilities::default()
        )))));
        let mut capabilities = AgentCapabilities::default();
        capabilities.prompt_capabilities.image = true;
        assert!(agent_accepts_prompt_images(&operational(Some(Box::new(
            capabilities
        )))));
    }

    #[test]
    fn phone_snapshot_projects_capability_gated_and_agent_commands_with_provenance() {
        use agent_client_protocol::schema::v1::{
            AvailableCommand, AvailableCommandInput, SessionMode, SessionModeState,
            UnstructuredCommandInput,
        };
        use hel::hel_worker::{RelayExecutionState, RelayOperationalState};
        use mj_controller::hel_server::ViewerCommandSource;

        let mut controller = controller_with_profiles(&["claude"]);
        let mut record = phone_session("session-1", 0);
        record.harness_kind = HarnessKind::Claude;
        record.last_profile = "claude".into();
        record.state = SessionState::Running;
        controller.state.sessions.insert(record.id.clone(), record);
        let operational = RelayOperationalState {
            session_id: "session-1".into(),
            execution: RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: String::new(),
            acknowledged_through: 0,
            acknowledged_digest: String::new(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: String::new(),
            native_session_id: None,
            agent_capabilities: None,
            agent_info: None,
            config_options: Vec::new(),
            modes: Some(SessionModeState::new(
                "default",
                vec![
                    SessionMode::new("default", "Default"),
                    SessionMode::new("plan", "Plan"),
                ],
            )),
            available_commands: vec![
                AvailableCommand::new("inspect", " Inspect the workspace ").input(
                    AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(" query ")),
                ),
                AvailableCommand::new("Review", "agent collision"),
                AvailableCommand::new("INSPECT", "duplicate agent command"),
            ],
            config: std::collections::BTreeMap::new(),
            active_prompt: None,
            queued_prompts: Vec::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            checkpoint_barrier: None,
            checkpoint_ready: None,
            last_acp_activity_at_ms: None,
            current_step_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            background_commands: Vec::new(),
        };
        let mut operational = std::collections::BTreeMap::from([("session-1".into(), operational)]);
        let project = |operational: &std::collections::BTreeMap<String, RelayOperationalState>| {
            viewer_snapshot(
                &controller,
                &[],
                &std::collections::BTreeMap::new(),
                &PhoneSessionViews {
                    conversations: &std::collections::BTreeMap::new(),
                    queued_prompts: &std::collections::BTreeMap::new(),
                    active_user_shells: &std::collections::BTreeMap::new(),
                    pending_elicitations: &std::collections::BTreeMap::new(),
                    prompt_images: &std::collections::BTreeSet::new(),
                    operational,
                    operations: &std::collections::BTreeMap::new(),
                    capacity: &[],
                    launch_failures: &[],
                    reviews: &std::collections::BTreeMap::new(),
                },
                1,
            )
        };
        let snapshot = project(&operational);
        let session = &snapshot.sessions[0];

        assert!(session.capabilities.prompt);
        assert!(session.capabilities.set_plan_mode);
        assert_eq!(
            session
                .available_commands
                .iter()
                .map(|command| (command.name.as_str(), command.source))
                .collect::<Vec<_>>(),
            vec![
                ("help", ViewerCommandSource::Mj),
                ("detach", ViewerCommandSource::Mj),
                ("plan", ViewerCommandSource::Mj),
                ("implement", ViewerCommandSource::Mj),
                ("review", ViewerCommandSource::Mj),
                ("inspect", ViewerCommandSource::Agent),
            ]
        );
        let inspect = session.available_commands.last().unwrap();
        assert_eq!(inspect.description, "Inspect the workspace");
        assert_eq!(inspect.argument.as_deref(), Some("query"));

        assert!(session.is_idle);
        assert_eq!(session.activity, "[idle]");
        let state = operational.get_mut("session-1").unwrap();
        state
            .background_commands
            .push(hel::hel_worker::BackgroundCommand {
                started_at_ms: 1_000,
                command: "background check".into(),
            });
        let background = project(&operational);
        assert!(!background.sessions[0].is_idle);
        assert!(background.sessions[0].activity.starts_with("BG "));
        let state = operational.get_mut("session-1").unwrap();
        state.background_commands.clear();
        // Execution is authoritative even before its timestamp arrives.
        state.execution = RelayExecutionState::Running;
        let running = project(&operational);
        assert!(!running.sessions[0].is_idle);
        assert_ne!(running.sessions[0].activity, "[idle]");
        operational.get_mut("session-1").unwrap().execution = RelayExecutionState::Idle;
        let idle_again = project(&operational);
        assert!(idle_again.sessions[0].is_idle);
        assert_eq!(idle_again.sessions[0].activity, "[idle]");
        let unknown = project(&std::collections::BTreeMap::new());
        assert!(!unknown.sessions[0].is_idle);
        assert!(unknown.sessions[0].activity.is_empty());
    }

    #[test]
    fn tailscale_listener_preserves_the_configured_port() {
        assert_eq!(
            tailscale_bind("127.0.0.1:4765".parse().unwrap()),
            "0.0.0.0:4765".parse().unwrap()
        );
    }

    fn controller_with_profiles(ids: &[&str]) -> Controller {
        Controller {
            config: HelConfig {
                version: CONFIG_VERSION,
                newer_config_version: None,
                phone: Default::default(),
                review: Default::default(),
                profiles: ids
                    .iter()
                    .map(|id| {
                        (
                            (*id).to_owned(),
                            HarnessProfile {
                                context_window_bytes: None,
                                kind: HarnessKind::Codex,
                                home: PathBuf::from("/home/agent").join(id),
                                environment: std::collections::BTreeMap::new(),
                            },
                        )
                    })
                    .collect(),
                bundles: std::collections::BTreeMap::new(),
                targets: std::collections::BTreeMap::new(),
            },
            state: HelState::default(),
        }
    }

    #[test]
    fn capacity_target_publication_skips_unchanged_targets_and_preserves_readings() {
        let mut controller = controller_with_profiles(&[]);
        controller
            .config
            .targets
            .insert("raw".into(), TargetTemplate::LocalBare);
        let (targets_tx, mut targets_rx) = tokio::sync::watch::channel(Vec::new());
        let mut state = std::collections::BTreeMap::new();

        publish_capacity_targets(&controller, &targets_tx, &mut state);
        assert!(targets_rx.has_changed().expect("target sender is alive"));
        assert_eq!(targets_rx.borrow_and_update().len(), 1);

        let usage = hel::hel_targets::DeploymentCapacityUsage {
            cpu_percent: Some(37),
            memory_used_bytes: 3,
            memory_total_bytes: 4,
            logical_cores: 8,
            disk_total_bytes: Some(5),
        };
        let local = state.get_mut("local").expect("local capacity state");
        local.usage = Some(usage.clone());
        local.on_demand = true;
        local.sampled_at_epoch_seconds = Some(42);
        local.refreshing = false;

        publish_capacity_targets(&controller, &targets_tx, &mut state);
        assert!(!targets_rx.has_changed().expect("target sender is alive"));
        let local_capacity = viewer_capacity(&state)
            .into_iter()
            .find(|capacity| capacity.id == "local")
            .expect("local viewer capacity");
        assert_eq!(local_capacity.cpu_percent, usage.cpu_percent);
        assert_eq!(
            local_capacity.memory_used_bytes,
            Some(usage.memory_used_bytes)
        );
        assert_eq!(local_capacity.logical_cores, Some(usage.logical_cores));
        assert_eq!(local_capacity.sampled_at_epoch_seconds, Some(42));

        controller
            .config
            .targets
            .insert("second-local".into(), TargetTemplate::LocalBare);
        publish_capacity_targets(&controller, &targets_tx, &mut state);
        assert!(targets_rx.has_changed().expect("target sender is alive"));
        assert_eq!(targets_rx.borrow_and_update().len(), 1);
        assert_eq!(state["local"].usage, Some(usage.clone()));

        controller.config.targets.insert(
            "fleet".into(),
            TargetTemplate::AwsEc2 {
                aws_profile: None,
                region: "us-east-1".into(),
                launch_template: "hel-runson".into(),
                launch_template_version: None,
                ssh_user: "ubuntu".into(),
                address_source: Default::default(),
                identity_file: None,
                ssh_args: Vec::new(),
            },
        );
        publish_capacity_targets(&controller, &targets_tx, &mut state);
        assert!(targets_rx.has_changed().expect("target sender is alive"));
        assert_eq!(targets_rx.borrow_and_update().len(), 2);
        assert!(state.contains_key("aws:fleet"));
        assert_eq!(state["local"].usage, Some(usage.clone()));

        controller.config.targets.remove("fleet");
        publish_capacity_targets(&controller, &targets_tx, &mut state);
        assert!(targets_rx.has_changed().expect("target sender is alive"));
        assert_eq!(targets_rx.borrow_and_update().len(), 1);
        assert!(!state.contains_key("aws:fleet"));
        assert_eq!(state["local"].usage, Some(usage));
    }

    fn prompt_action() -> ControllerAction {
        ControllerAction::Prompt {
            session_id: "session-1".into(),
            text: "ship it".into(),
            images: Vec::new(),
        }
    }

    fn new_action() -> ControllerAction {
        ControllerAction::New {
            workspace_id: String::new(),
            profile_id: "codex".into(),
            bundle_id: "project".into(),
            target_id: "podman".into(),
            title: Some("Phone launch".into()),
            project_directory: None,
            dirty_ack: Vec::new(),
        }
    }

    fn phone_session(id: &str, viewed_through_event_ordinal: u64) -> SessionRecord {
        SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: id.into(),
            title: "Phone launch".into(),
            harness_kind: hel::hel_config::HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Provisioning,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: Some("Phone launch".into()),
            created_at: "2026-08-14T00:00:00Z".into(),
            updated_at: "2026-08-14T00:00:00Z".into(),
            viewed_through_event_ordinal,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }

    #[tokio::test]
    async fn controller_reload_does_not_block_the_phone_control_loop() {
        let (completed_tx, mut completed_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let loaded = controller_with_profiles(&[]);
        let release = std::thread::spawn(move || {
            started_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(250));
            release_tx.send(()).unwrap();
        });

        let started = Instant::now();
        spawn_controller_reload_with(completed_tx, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(loaded)
        });
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "scheduling a controller reload occupied the control loop for {:?}",
            started.elapsed()
        );

        let completed = tokio::time::timeout(Duration::from_secs(1), completed_rx.recv())
            .await
            .expect("background reload timed out")
            .expect("background reload channel closed");
        assert!(completed.result.is_ok());
        release.join().unwrap();
    }

    #[test]
    fn read_receipt_only_persists_and_refreshes_when_the_cursor_advances() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut state = HelState::default();
        state
            .sessions
            .insert(session_id.into(), phone_session(session_id, 5));

        // The viewer re-posts its cursor after every refresh; a repeat must
        // not reach the database and must not move the revision.
        assert_eq!(
            plan_read_receipt(&state, session_id, 5),
            ReadReceiptPlan::AlreadyRead
        );
        assert_eq!(
            plan_read_receipt(&state, session_id, 4),
            ReadReceiptPlan::AlreadyRead
        );
        assert_eq!(
            plan_read_receipt(&state, "missing", 9),
            ReadReceiptPlan::UnknownSession
        );
        assert_eq!(
            plan_read_receipt(&state, session_id, 9),
            ReadReceiptPlan::Persist
        );

        assert!(apply_read_receipt(&mut state, session_id, 9));
        assert_eq!(state.sessions[session_id].viewed_through_event_ordinal, 9);
        assert!(!apply_read_receipt(&mut state, session_id, 9));
        assert!(!apply_read_receipt(&mut state, session_id, 7));
        assert!(!apply_read_receipt(&mut state, "missing", 9));
        assert_eq!(
            plan_read_receipt(&state, session_id, 9),
            ReadReceiptPlan::AlreadyRead
        );
    }

    #[tokio::test]
    async fn an_admitted_action_answers_its_phone_before_the_work_runs() {
        let mut replies = PendingActionReplies::default();
        let (reply, answer) = tokio::sync::oneshot::channel();

        replies.accept(1, &prompt_action(), reply);

        // No completion has been reported, and the phone already has its
        // answer: holding it until the action finished is what mobile
        // networks time out on.
        assert_eq!(answer.await.unwrap(), ActionOutcome::Accepted);
    }

    #[tokio::test]
    async fn a_new_action_answers_once_its_provisional_session_is_published() {
        let mut replies = PendingActionReplies::default();
        let (reply, mut answer) = tokio::sync::oneshot::channel();

        replies.accept(7, &new_action(), reply);
        assert!(
            matches!(
                answer.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "a new session has no id to report before it is published"
        );

        replies.resolve(7, ActionOutcome::Accepted);
        assert_eq!(answer.await.unwrap(), ActionOutcome::Accepted);
    }

    #[tokio::test]
    async fn a_new_action_that_never_publishes_still_answers_its_phone() {
        let mut replies = PendingActionReplies::default();
        let (reply, answer) = tokio::sync::oneshot::channel();
        replies.accept(7, &new_action(), reply);

        // Registration failed before the session reached the loop, which is
        // the completion path rather than the publication path.
        replies.resolve(7, ActionOutcome::Failed);

        assert_eq!(answer.await.unwrap(), ActionOutcome::Failed);
        // A second resolution is a no-op, so a completion after a publication
        // cannot overwrite the answer already sent.
        replies.resolve(7, ActionOutcome::Accepted);
    }

    #[test]
    fn a_refused_action_reports_the_reason_the_phone_can_act_on() {
        let mut active = std::collections::BTreeSet::new();

        assert_eq!(
            admit_phone_action(&prompt_action(), 0, &mut active),
            Ok(Some("session-1".to_owned()))
        );
        assert_eq!(
            admit_phone_action(&prompt_action(), 1, &mut active),
            Err(ActionOutcome::SessionBusy)
        );
        assert_eq!(
            admit_phone_action(&new_action(), MAX_CONCURRENT_PHONE_ACTIONS, &mut active),
            Err(ActionOutcome::Busy)
        );
        // A refusal must not consume the session slot it did not take.
        assert_eq!(active.len(), 1);
        assert_eq!(admit_phone_action(&new_action(), 1, &mut active), Ok(None));
    }

    #[test]
    fn a_feed_that_ends_outside_shutdown_names_the_failure() {
        assert!(feed_stopped(true, "the session manager stopped").is_none());
        let failure = feed_stopped(false, "the session manager stopped").expect("named failure");
        assert!(failure.to_string().contains("session manager"));
    }

    #[test]
    fn a_profile_added_while_the_server_runs_reaches_the_quota_refresher() {
        let (profiles_tx, profiles_rx) = tokio::sync::watch::channel(QuotaRefreshBatch::default());
        let mut published = std::collections::BTreeMap::new();
        let mut batch = QuotaRefreshBatch::default();
        let controller = controller_with_profiles(&["codex"]);

        assert!(republish_quota_profiles(
            &controller,
            &mut published,
            &mut batch,
            &profiles_tx
        ));
        assert_eq!(
            profiles_rx
                .borrow()
                .profiles
                .iter()
                .map(|profile| profile.profile_id.clone())
                .collect::<Vec<_>>(),
            vec!["codex".to_owned()]
        );
        let first_generation = profiles_rx.borrow().generation;

        // Every finished action reloads the configuration; an unchanged one
        // must not restart a harness process per profile.
        assert!(!republish_quota_profiles(
            &controller,
            &mut published,
            &mut batch,
            &profiles_tx
        ));
        assert_eq!(profiles_rx.borrow().generation, first_generation);

        let grown = controller_with_profiles(&["claude", "codex"]);
        assert!(republish_quota_profiles(
            &grown,
            &mut published,
            &mut batch,
            &profiles_tx
        ));
        assert_eq!(
            profiles_rx
                .borrow()
                .profiles
                .iter()
                .map(|profile| profile.profile_id.clone())
                .collect::<Vec<_>>(),
            vec!["claude".to_owned(), "codex".to_owned()]
        );
        assert!(profiles_rx.borrow().generation > first_generation);
    }

    #[test]
    fn a_quota_reads_stale_only_once_its_next_refresh_is_overdue() {
        let controller = controller_with_profiles(&["codex"]);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let quota_refreshed = |age: Duration| {
            let quotas = std::collections::BTreeMap::from([(
                "codex".to_owned(),
                ProfileQuota {
                    profile_id: "codex".into(),
                    harness: HarnessKind::Codex,
                    windows: Vec::new(),
                    extra: None,
                    error: None,
                    refreshed_at_epoch_seconds: now - age.as_secs(),
                },
            )]);
            viewer_snapshot(
                &controller,
                &[],
                &quotas,
                &PhoneSessionViews {
                    conversations: &std::collections::BTreeMap::new(),
                    queued_prompts: &std::collections::BTreeMap::new(),
                    active_user_shells: &std::collections::BTreeMap::new(),
                    pending_elicitations: &std::collections::BTreeMap::new(),
                    prompt_images: &std::collections::BTreeSet::new(),
                    operational: &std::collections::BTreeMap::new(),
                    operations: &std::collections::BTreeMap::new(),
                    capacity: &[],
                    launch_failures: &[],
                    reviews: &std::collections::BTreeMap::new(),
                },
                1,
            )
            .profiles[0]
                .quota
                .as_ref()
                .expect("the profile carries its quota")
                .stale
        };

        // A reading taken one refresh interval ago is exactly what a healthy
        // refresher produces, so it must not be labelled stale.
        assert!(!quota_refreshed(QUOTA_REFRESH_INTERVAL));
        assert!(!quota_refreshed(QUOTA_STALE_AFTER));
        assert!(quota_refreshed(QUOTA_STALE_AFTER + Duration::from_secs(1)));
    }

    #[test]
    fn phone_action_capacity_is_bounded() {
        assert!(phone_action_capacity_available(
            MAX_CONCURRENT_PHONE_ACTIONS - 1
        ));
        assert!(!phone_action_capacity_available(
            MAX_CONCURRENT_PHONE_ACTIONS
        ));
    }

    #[test]
    fn started_phone_session_is_visible_and_mapped_before_provisioning() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let session = phone_session(session_id, 0);
        let mut state = HelState::default();
        let mut active_actions = std::collections::BTreeSet::new();
        let mut action_sessions = std::collections::BTreeMap::new();

        track_started_phone_session(
            &mut state,
            &mut active_actions,
            &mut action_sessions,
            7,
            session,
        )
        .unwrap();

        assert_eq!(state.sessions[session_id].state, SessionState::Provisioning);
        assert_eq!(state.sessions[session_id].display_title(), "Phone launch");
        assert!(active_actions.contains(session_id));
        assert_eq!(
            action_sessions.get(&7).map(String::as_str),
            Some(session_id)
        );
    }

    #[test]
    fn failed_launch_notice_survives_session_rollback_and_history_is_bounded() {
        let controller = controller_with_profiles(&["codex"]);
        let mut failures = Vec::new();
        for index in 0..20 {
            record_launch_failure(&mut failures, index, format!("workspace-{index}"));
        }
        let snapshot = viewer_snapshot(
            &controller,
            &[],
            &std::collections::BTreeMap::new(),
            &PhoneSessionViews {
                conversations: &std::collections::BTreeMap::new(),
                queued_prompts: &std::collections::BTreeMap::new(),
                active_user_shells: &std::collections::BTreeMap::new(),
                pending_elicitations: &std::collections::BTreeMap::new(),
                prompt_images: &std::collections::BTreeSet::new(),
                operational: &std::collections::BTreeMap::new(),
                operations: &std::collections::BTreeMap::new(),
                capacity: &[],
                launch_failures: &failures,
                reviews: &std::collections::BTreeMap::new(),
            },
            1,
        );
        assert!(snapshot.sessions.is_empty());
        assert_eq!(failures.len(), 16);
        assert_eq!(failures[0].workspace_id, "workspace-4");
        assert_eq!(failures[15].workspace_id, "workspace-19");
        assert_ne!(failures[0].id, failures[1].id);
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["launch_failures"][15]["workspace_id"], "workspace-19");
        assert_eq!(json["launch_failures"][15].as_object().unwrap().len(), 2);
    }

    #[test]
    fn phone_cancel_targets_the_matching_background_action() {
        let first = PhoneActionControl {
            cancelled: Arc::new(AtomicBool::new(false)),
            new_gate: None,
        };
        let second = PhoneActionControl {
            cancelled: Arc::new(AtomicBool::new(false)),
            new_gate: None,
        };
        let action_sessions =
            std::collections::BTreeMap::from([(1, "session-1".into()), (2, "session-2".into())]);
        let cancellations =
            std::collections::BTreeMap::from([(1, first.clone()), (2, second.clone())]);

        assert!(request_phone_action_cancellation(
            "session-2",
            &action_sessions,
            &cancellations,
        ));
        assert!(!first.cancelled.load(Ordering::Acquire));
        assert!(second.cancelled.load(Ordering::Acquire));
        assert!(!request_phone_action_cancellation(
            "missing",
            &action_sessions,
            &cancellations,
        ));
    }

    #[test]
    fn phone_new_cancel_and_running_commit_have_one_atomic_winner() {
        for _ in 0..100 {
            let control = PhoneActionControl {
                cancelled: Arc::new(AtomicBool::new(false)),
                new_gate: Some(Arc::new(PhoneNewActionGate::new())),
            };
            let cancelling = control.clone();
            let committing = control.clone();
            let (cancelled, committed) = std::thread::scope(|scope| {
                let cancel = scope.spawn(move || cancelling.request_cancel());
                let commit = scope.spawn(move || committing.grant_new_commit());
                (cancel.join().unwrap(), commit.join().unwrap())
            });

            assert_ne!(cancelled, committed);
            assert_eq!(control.cancelled.load(Ordering::Acquire), cancelled);
            assert!(!control.request_cancel());
            assert!(!control.grant_new_commit());
        }
    }

    fn materialized_at(ordinal: u64) -> MaterializedSession {
        let mut materialized = MaterializedSession::empty("session-1");
        materialized.applied_event_ordinal = ordinal;
        materialized.applied_event_digest = format!("digest-{ordinal}");
        materialized
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn browser_projection_enqueue_does_not_wait_for_a_blocking_permit() {
        let (results, mut completed) = tokio::sync::mpsc::channel(1);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut dispatcher = ConversationProjectionDispatcher::with_permits(results, shutdown, 0);

        dispatcher.enqueue(materialized_at(1));
        let (progress, progress_done) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            progress.send(()).expect("control task is still alive");
        });

        tokio::time::timeout(Duration::from_millis(100), progress_done)
            .await
            .expect("control progress was starved by transcript projection")
            .expect("progress task did not report");
        assert!(completed.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn browser_projection_keeps_only_the_newest_pending_snapshot() {
        let (results, mut completed) = tokio::sync::mpsc::channel(4);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut dispatcher = ConversationProjectionDispatcher::with_permits(results, shutdown, 0);

        dispatcher.enqueue(materialized_at(1));
        dispatcher.enqueue(materialized_at(3));
        dispatcher.enqueue(materialized_at(2));
        assert_eq!(dispatcher.pending["session-1"].key.ordinal, 3);

        dispatcher.permits.add_permits(1);
        let first = tokio::time::timeout(Duration::from_secs(1), completed.recv())
            .await
            .expect("first projection did not complete")
            .expect("projection result channel closed");
        assert_eq!(first.key.ordinal, 1);
        assert!(dispatcher.finish(first, true).is_some());

        let second = tokio::time::timeout(Duration::from_secs(1), completed.recv())
            .await
            .expect("coalesced projection did not complete")
            .expect("projection result channel closed");
        assert_eq!(second.key.ordinal, 3);
        assert!(dispatcher.finish(second, true).is_some());
        assert!(dispatcher.pending.is_empty());
        assert!(dispatcher.in_flight.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn forgotten_projection_cannot_repopulate_after_a_same_cursor_resume() {
        let (results, mut completed) = tokio::sync::mpsc::channel(4);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut dispatcher = ConversationProjectionDispatcher::with_permits(results, shutdown, 0);
        let snapshot = materialized_at(7);

        dispatcher.enqueue(snapshot.clone());
        dispatcher.forget("session-1");
        dispatcher.enqueue(snapshot);
        dispatcher.permits.add_permits(1);

        let old = tokio::time::timeout(Duration::from_secs(1), completed.recv())
            .await
            .expect("old projection did not complete")
            .expect("projection result channel closed");
        assert!(dispatcher.finish(old, true).is_none());

        let current = tokio::time::timeout(Duration::from_secs(1), completed.recv())
            .await
            .expect("resumed projection did not complete")
            .expect("projection result channel closed");
        assert!(dispatcher.finish(current, true).is_some());
        assert!(dispatcher.in_flight.is_empty());
    }
}
