use std::path::Path;
mod antigravity;
mod dispatch;
pub(super) mod git_env;
mod kimi;
mod requests;
mod supervisor;
use antigravity::*;
pub(crate) use dispatch::*;
pub use git_env::*;
pub(crate) use kimi::*;
pub(crate) use requests::*;
pub use supervisor::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::SessionUpdate;
use anyhow::{Context, Result, bail};
use mj_core::local_sockets::{bind_unix_listener, connect_unix_stream};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::review_capture::ReviewCapture;
use super::{
    AcpSupervisorSpec, CredentialEndpoint, WorkerLaunchConfig, reviewer_workspace_directories,
};

use crate::acp::{self, CommandRequest, LaunchSpec, RuntimeEvent};
use crate::relay::{
    ClaimedRelayCommand, DeferredRelayAttach, DurableRelay, NativeSessionHistory, RELAY_STATE_FILE,
    RESTORED_RELAY_SEED_FILE, RelayCell, RelayCommand, RelayCommandOutcome, RelayErrorCode,
    RelayObservation, RelayProtocolError, RelayRequest, RelayRequestEnvelope, RelayResponseBody,
    RelayResponseEnvelope, RelayResponsePayload, invalid_relay_request_response,
    unsupported_relay_method_response,
};
use mj_core::config::HarnessKind;
use mj_core::subprocess::terminate_process_group;
use mj_core::worker_protocol::{DecodedRelayRequest, decode_relay_request};

pub(crate) const ACP_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Cold local filesystems can take minutes to traverse under host pressure.
/// Subagent profile, project memory, ACP setup, and bridge start each get
/// three minutes, with no local preparation stage exceeding five minutes.
const LOCAL_PREPARATION_STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3 * 60);

#[derive(Clone, Default)]
pub(super) struct PreparationSnapshot {
    state: Option<mj_core::relay::HarnessPreparation>,
    services: Option<Arc<PreparedConnectionServices>>,
    /// The current step's own bound, which also bounds the threads it starts.
    step_deadline: Option<tokio::time::Instant>,
}

impl PreparationSnapshot {
    fn is_pending(&self) -> bool {
        !matches!(
            self.state,
            Some(mj_core::relay::HarnessPreparation::Started)
        )
    }
}

#[derive(Clone)]
struct PreparedConnectionServices {
    commands: mpsc::Sender<CommandRequest>,
    reviewer: Arc<ReviewCapture>,
    subagents: Option<super::subagents::SubagentEndpoint>,
}

#[derive(Clone, Default)]
pub(super) struct SharedSubagents(Arc<Mutex<SharedSubagentsState>>);

#[derive(Default)]
struct SharedSubagentsState {
    endpoint: Option<super::subagents::SubagentEndpoint>,
    guard: Option<super::subagents::SubagentSocketGuard>,
    closed: bool,
}

impl SharedSubagents {
    fn endpoint(&self) -> Option<super::subagents::SubagentEndpoint> {
        self.0
            .lock()
            .expect("sub-agent owner lock poisoned")
            .endpoint
            .clone()
    }

    fn ensure_serving(
        &self,
        runtime: &tokio::runtime::Handle,
        root: &std::path::Path,
        relay: Arc<RelayCell>,
        harness: HarnessKind,
        subagent_kind: Option<mj_core::subagent::SubagentKind>,
        preparation: Option<&watch::Sender<PreparationSnapshot>>,
    ) -> Result<super::subagents::SubagentEndpoint> {
        let endpoint = {
            let mut state = self.0.lock().expect("sub-agent owner lock poisoned");
            if let Some(endpoint) = &state.endpoint {
                return Ok(endpoint.clone());
            }
            if state.closed {
                bail!("worker is closing; cannot start sub-agent socket");
            }
            let (endpoint, guard) =
                super::subagents::serve(runtime, root, relay, harness, subagent_kind)?;
            state.endpoint = Some(endpoint.clone());
            state.guard = Some(guard);
            endpoint
        };
        if let Some(preparation) = preparation {
            preparation.send_modify(|snapshot| {
                if let Some(services) = snapshot.services.as_mut() {
                    let mut updated = (**services).clone();
                    updated.subagents = Some(endpoint.clone());
                    *services = Arc::new(updated);
                }
            });
        }
        Ok(endpoint)
    }

    fn stop(&self) {
        let guard = {
            let mut state = self.0.lock().expect("sub-agent owner lock poisoned");
            state.closed = true;
            state.guard.take()
        };
        drop(guard);
    }
}

struct SharedSubagentsShutdownGuard(SharedSubagents);

impl Drop for SharedSubagentsShutdownGuard {
    fn drop(&mut self) {
        self.0.stop();
    }
}

struct RunningHarness {
    acp_task: tokio::task::JoinHandle<Result<()>>,
    event_task: tokio::task::JoinHandle<Result<()>>,
    acp_shutdown: CancellationToken,
    commands: mpsc::Sender<CommandRequest>,
    _reviewer: Arc<ReviewCapture>,
    subagent_socket_guard: Option<SharedSubagentsShutdownGuard>,
    _subagents: Option<super::subagents::SubagentEndpoint>,
    harness_gc: Option<tokio::task::JoinHandle<()>>,
    shell_cleanup: tokio_util::task::TaskTracker,
    /// Runtime leases stay in long-lived ACP state so they cover supervisor
    /// replacement gaps as well as the lifetime of any individual supervisor.
    _managed_harness: Option<super::harness::ManagedHarness>,
    _managed_node: Option<super::harness::ManagedNodeRuntime>,
    /// Stopped when the harness ends, before a closed session is checkpointed.
    project_tools: Option<super::project_tools::BackgroundInstall>,
}

/// How a harness preparation ended. There is no failed state to serve: a
/// harness that cannot start ends the worker.
enum PreparationOutcome {
    Started(Box<RunningHarness>),
    /// A Close sealed the relay, or the worker is stopping, before the bridge
    /// started. The receiver keeps request-side dispatch wakes deliverable
    /// while the sealed relay is served without a harness.
    Cancelled(Option<mpsc::Receiver<()>>),
    /// The harness cannot start. The error carries a
    /// [`mj_core::relay::WorkerHarnessStartupFailure`] for the exit record.
    Failed(anyhow::Error),
}

/// Why [`supervise_preparation`] did not return a prepared value.
#[derive(Debug)]
enum PreparationStop {
    Cancelled,
    Failed(anyhow::Error),
}

struct HarnessPreparationSetup {
    shared_subagents: SharedSubagents,
    antigravity_refresh: mpsc::Receiver<oneshot::Sender<Option<u64>>>,
    root: PathBuf,
    config: WorkerLaunchConfig,
    relay: Arc<RelayCell>,
    restarting: bool,
    task_monitors: TaskMonitors,
    resume_session: Option<String>,
    native_session_history: NativeSessionHistory,
    /// What a sub-agent's worker observes at the end of each turn.
    capture: Option<crate::work_facts::WorkObserver>,
}

#[derive(Clone)]
struct PreparationStepBudget {
    step: String,
    started_at: chrono::DateTime<chrono::Utc>,
    deadline: tokio::time::Instant,
    timeout: std::time::Duration,
}

impl PreparationStepBudget {
    fn new(step: &str, timeout: std::time::Duration) -> Self {
        let started_at = chrono::Utc::now();
        let _ = ACTIVE_PREPARATION.try_with(|gate| gate.set_step(step, started_at));
        Self {
            step: step.to_owned(),
            started_at,
            deadline: tokio::time::Instant::now() + timeout,
            timeout,
        }
    }

    fn remaining(&self) -> std::time::Duration {
        self.deadline
            .saturating_duration_since(tokio::time::Instant::now())
    }

    fn deadline_error(&self) -> anyhow::Error {
        anyhow::anyhow!(
            "worker preparation step {} exceeded its {:?} deadline",
            self.step,
            self.timeout
        )
    }
}

struct AcpPreparationSetup {
    shared_subagents: SharedSubagents,
    root: PathBuf,
    config: WorkerLaunchConfig,
    session_environment: BTreeMap<String, String>,
    supervisor_spec: AcpSupervisorSpec,
    git_setup: tempfile::TempDir,
    relay: Arc<RelayCell>,
    resume_session: Option<String>,
    native_session_history: NativeSessionHistory,
    profile_registration: bool,
    /// Read from the launch before preparation strips the marker from the
    /// harness environment, so the ACP runtime knows the session is a child.
    managed_subagent: bool,
    subagent_role: Option<mj_core::subagent::SubagentMcpRole>,
    runtime: tokio::runtime::Handle,
    managed_cache_root: Option<PathBuf>,
}

struct PreparedAcpSetup {
    commands_tx: mpsc::Sender<CommandRequest>,
    commands_rx: mpsc::Receiver<CommandRequest>,
    events_tx: mpsc::Sender<RuntimeEvent>,
    events_rx: mpsc::Receiver<RuntimeEvent>,
    user_shells: crate::user_shell::UserShellRegistry,
    reviewer: Arc<ReviewCapture>,
    subagents: Option<super::subagents::SubagentEndpoint>,
    subagent_socket_guard: Option<SharedSubagentsShutdownGuard>,
    acp_spec: LaunchSpec,
    shell_cleanup: tokio_util::task::TaskTracker,
    managed_cache_root: Option<PathBuf>,
    harness: HarnessKind,
}

struct StartedBridgeTasks {
    acp_shutdown: Option<CancellationToken>,
    acp_task: Option<tokio::task::JoinHandle<Result<()>>>,
    event_task: Option<tokio::task::JoinHandle<Result<()>>>,
    harness_gc: Option<tokio::task::JoinHandle<()>>,
    commands: Option<mpsc::Sender<CommandRequest>>,
    reviewer: Option<Arc<ReviewCapture>>,
    subagent_socket_guard: Option<SharedSubagentsShutdownGuard>,
    subagents: Option<super::subagents::SubagentEndpoint>,
    shell_cleanup: Option<tokio_util::task::TaskTracker>,
}

impl StartedBridgeTasks {
    fn into_running(mut self) -> RunningHarness {
        RunningHarness {
            acp_shutdown: self.acp_shutdown.take().expect("ACP task was started"),
            acp_task: self.acp_task.take().expect("ACP task was started"),
            event_task: self
                .event_task
                .take()
                .expect("relay coordinator was started"),
            harness_gc: self.harness_gc.take(),
            commands: self
                .commands
                .take()
                .expect("ACP command sender was prepared"),
            _reviewer: self.reviewer.take().expect("reviewer was prepared"),
            subagent_socket_guard: self.subagent_socket_guard.take(),
            _subagents: self.subagents.take(),
            shell_cleanup: self
                .shell_cleanup
                .take()
                .expect("shell tracker was prepared"),
            _managed_harness: None,
            _managed_node: None,
            project_tools: None,
        }
    }
}

impl Drop for StartedBridgeTasks {
    fn drop(&mut self) {
        if let Some(shutdown) = &self.acp_shutdown {
            shutdown.cancel();
        }
        if let Some(task) = self.acp_task.take() {
            task.abort();
        }
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
        if let Some(task) = self.harness_gc.take() {
            task.abort();
        }
    }
}

fn build_acp_setup(setup: AcpPreparationSetup) -> Result<PreparedAcpSetup> {
    let AcpPreparationSetup {
        shared_subagents,
        root,
        config,
        session_environment,
        supervisor_spec,
        git_setup,
        relay,
        resume_session,
        native_session_history,
        profile_registration,
        managed_subagent,
        subagent_role,
        runtime,
        managed_cache_root,
    } = setup;
    let (commands_tx, commands_rx) = mpsc::channel(32);
    let (events_tx, events_rx) = mpsc::channel(ACP_EVENT_CHANNEL_CAPACITY);
    let user_shells = crate::user_shell::UserShellRegistry::new(
        config.cwd.clone(),
        session_environment.clone(),
        events_tx.clone(),
    );
    let accepted_config = {
        let relay = relay.lock().expect("relay lock poisoned");
        let state = relay.operational_state();
        acp::AcceptedSessionConfig::from_configuration(&state.config, &state.config_options)
    };
    let supervisor_path = root.join("acp-supervisor.json");
    supervisor_spec.write_spec(&supervisor_path)?;
    publish_git_setup(&root, git_setup)?;
    let worker_executable = std::env::current_exe().context("locate Hel worker executable")?;
    let reviewer = Arc::new(ReviewCapture {
        session_id: config.session_id.clone(),
        cwd: config.cwd.clone(),
        additional_directories: reviewer_workspace_directories(&config),
        primary_relay: relay.clone(),
        operation: tokio::sync::Mutex::new(()),
    });
    let subagent_socket_guard = Some(SharedSubagentsShutdownGuard(shared_subagents.clone()));
    let subagents = if subagent_role.is_some() {
        Some(shared_subagents.ensure_serving(
            &runtime,
            &root,
            relay.clone(),
            config.harness,
            config.subagent_kind,
            None,
        )?)
    } else {
        None
    };

    let (
        acp_activity,
        step_clock,
        tools_in_flight,
        turn_context,
        accepted_config,
        login_identity,
        operator_away,
    ) = {
        let relay = relay.lock().expect("relay lock poisoned");
        (
            relay.acp_activity_clock(),
            relay.step_clock(),
            relay.tools_in_flight(),
            relay.turn_context(),
            Arc::new(Mutex::new(accepted_config)),
            relay.login_identity.clone(),
            relay.operator_away.clone(),
        )
    };
    let shell_cleanup = user_shells.completion_tracker();
    if let Some(request) = &config.goal_resume_request {
        let mut state = relay.lock().expect("relay lock poisoned");
        if state.operational_state().goal.answered_resume.as_ref() != Some(request) {
            state.record_session_update(serde_json::from_value(serde_json::json!({
                "sessionUpdate": "session_info_update",
                "_meta": {"mjGoalResumePending": request}
            }))?)?;
        }
    }
    let goal_recovery = Arc::new(Mutex::new(mj_core::goal::GoalRecoveryContext {
        state: relay
            .lock()
            .expect("relay lock poisoned")
            .operational_state()
            .goal,
        request: config.goal_resume_request.clone(),
        journal: Some(mj_core::goal::GoalJournal({
            let relay = relay.clone();
            Arc::new(move |update| {
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .record_session_update(update)?;
                Ok(())
            })
        })),
    }));
    let additional_directories = super::acp_session_directories(&config, &session_environment);
    let instruction_role = crate::acp::InstructionRole::from_launch(&config);
    let approval_choices = config.approval_choices();
    let acp_spec = LaunchSpec {
        operator_away,
        imported_logins: None,
        login_identity,
        clear_context_request: None,
        context_restore: None,
        goal_recovery,
        command: worker_executable,
        args: vec![
            "--login-environment-ready".into(),
            "worker".into(),
            "acp-supervisor".into(),
            "--spec".into(),
            supervisor_path.to_string_lossy().into_owned(),
        ],
        environment: session_environment,
        bridge_spec_path: Some(supervisor_path.clone()),
        cwd: config.cwd.clone(),
        additional_directories,
        extra_mcp_servers: crate::acp::launch_extra_mcp_servers(&config),
        subagent_policy: crate::acp::launch_subagent_policy(&config),
        managed_subagent,
        instruction_role,
        subagent_mcp_socket: crate::acp::launch_subagent_mcp_socket(
            &root,
            &config,
            subagent_role,
            profile_registration,
        ),
        project_memory: config.project_memory,
        resume_session,
        native_session_history,
        accepted_config,
        initial_model: config.initial_model,
        harness: config.harness,
        execution_policy: config.execution_policy,
        approval_choices,
        acp_activity,
        step_clock,
        tools_in_flight,
        terminal_owner: Some(Arc::downgrade(&relay)),
        turn_context,
        verdict: None,
        stall_policy: None,
    };

    Ok(PreparedAcpSetup {
        commands_tx,
        commands_rx,
        events_tx,
        events_rx,
        user_shells,
        reviewer,
        subagents,
        subagent_socket_guard,
        acp_spec,
        shell_cleanup,
        managed_cache_root,
        harness: config.harness,
    })
}

/// Hand relay-local lifecycle work from the request handler to the
/// coordinator about to start, or refuse because a Close sealed the relay
/// first. Returns the dispatch wakes the coordinator will own.
///
/// The relay lock is the one decision point between this and a Close the
/// request handler accepts during preparation (see `serve_client_with_memory`).
/// Started is published inside it, and only while preparation is not
/// cancelled and the relay is not sealed. A handler that saw Preparing
/// therefore completed the Close and cancelled a preparation that never starts
/// a bridge, and one that sees Started leaves the Close to the coordinator.
///
/// A seal found here cancels the preparation itself, under the same lock: the
/// handler that accepted the Close may not have cancelled it yet, and a worker
/// that booted with a closing relay has no handler to do it. The cancelled
/// token is then the one fact that makes this refusal a cancellation rather
/// than a startup failure. On refusal the wakes stay with the preparation for
/// the sealed relay.
fn publish_started(
    relay: &RelayCell,
    cancel: &CancellationToken,
    status: &watch::Sender<PreparationSnapshot>,
    dispatch_wakes: &mut Option<mpsc::Receiver<()>>,
    services: PreparedConnectionServices,
) -> Result<mpsc::Receiver<()>> {
    let state = relay.lock().expect("relay state lock poisoned");
    if state.sealed_by_close() {
        cancel.cancel();
    }
    if cancel.is_cancelled() {
        bail!("preparation cancelled before the bridge started");
    }
    let dispatch_wakes = dispatch_wakes
        .take()
        .context("preparation lost ownership of relay dispatch wake channel")?;
    status.send_replace(PreparationSnapshot {
        state: Some(mj_core::relay::HarnessPreparation::Started),
        services: Some(Arc::new(services)),
        step_deadline: None,
    });
    Ok(dispatch_wakes)
}

/// Start the bridge and its coordinator once [`publish_started`] accepts the
/// handoff. Started is published before any bridge process exists, and
/// nothing after the publication can fail, so a Started worker always has a
/// coordinator.
#[allow(clippy::too_many_arguments)]
fn start_bridge(
    setup: PreparedAcpSetup,
    antigravity_refresh: mpsc::Receiver<oneshot::Sender<Option<u64>>>,
    relay: Arc<RelayCell>,
    dispatch_wakes: &mut Option<mpsc::Receiver<()>>,
    task_monitors: TaskMonitors,
    capture: Option<crate::work_facts::WorkObserver>,
    runtime: tokio::runtime::Handle,
    cancel: &CancellationToken,
    status: &watch::Sender<PreparationSnapshot>,
) -> Result<StartedBridgeTasks> {
    let PreparedAcpSetup {
        commands_tx,
        commands_rx,
        events_tx,
        events_rx,
        user_shells,
        reviewer,
        subagents,
        subagent_socket_guard,
        acp_spec,
        shell_cleanup,
        managed_cache_root,
        harness,
    } = setup;
    let dispatch_wakes = publish_started(
        &relay,
        cancel,
        status,
        dispatch_wakes,
        PreparedConnectionServices {
            commands: commands_tx.clone(),
            reviewer: reviewer.clone(),
            subagents: subagents.clone(),
        },
    )?;
    let acp_shutdown = CancellationToken::new();
    let mut started = StartedBridgeTasks {
        acp_shutdown: Some(acp_shutdown.clone()),
        acp_task: None,
        event_task: None,
        harness_gc: managed_cache_root
            .map(|root| super::harness::spawn_gc_on(&runtime, root, harness)),
        commands: Some(commands_tx.clone()),
        reviewer: Some(reviewer),
        subagent_socket_guard,
        subagents,
        shell_cleanup: Some(shell_cleanup),
    };
    started.acp_task = Some(runtime.spawn(acp::run_with_guardian(
        acp_spec,
        commands_rx,
        events_tx,
        acp_shutdown,
        Some(relay.guardian.clone()),
    )));
    started.event_task = Some(runtime.spawn(run_relay_coordinator_with_shells(
        relay,
        events_rx,
        dispatch_wakes,
        commands_tx,
        user_shells,
        task_monitors,
        capture,
        HarnessStartup::new(),
        Some(antigravity_refresh),
    )));
    Ok(started)
}

struct AbortTaskOnDrop(tokio::task::AbortHandle);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct CancelPreparationOnDrop(CancellationToken);

impl Drop for CancelPreparationOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn preparation_step(
    root: &std::path::Path,
    status: Option<&watch::Sender<PreparationSnapshot>>,
    step: &str,
    timeout: std::time::Duration,
    cancel: &CancellationToken,
) -> Result<PreparationStepBudget> {
    let budget = PreparationStepBudget::new(step, timeout);
    let Some(status) = status else {
        return Ok(budget);
    };
    status.send_modify(|snapshot| {
        snapshot.state = Some(mj_core::relay::HarnessPreparation::Preparing {
            step: step.to_owned(),
            since_ms: budget.started_at.timestamp_millis(),
        });
        snapshot.step_deadline = Some(budget.deadline);
    });
    let root = root.to_owned();
    let step = step.to_owned();
    bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
        if step_cancel.is_cancelled() {
            anyhow::bail!("preparation cancelled before startup breadcrumb");
        }
        super::record_startup_step(&root, &step);
        Ok(())
    })
    .await?;
    Ok(budget)
}

fn preparation_error(snapshot: &PreparationSnapshot, operation: &str) -> Option<String> {
    match snapshot.state.as_ref() {
        Some(mj_core::relay::HarnessPreparation::Preparing { step, .. }) => Some(format!(
            "harness is preparing at {step}; {operation} is not available yet"
        )),
        // Standalone/reviewer socket handlers have no primary preparation
        // supervisor (`None`); their explicit services are already available.
        // This worker never reports `Failed`: a harness that cannot start
        // ends the worker instead.
        Some(
            mj_core::relay::HarnessPreparation::Started
            | mj_core::relay::HarnessPreparation::Failed { .. },
        )
        | None => None,
    }
}

/// Hold a `WaitForChange` until the relay's latest event or operational state,
/// or this connection's sub-agent queue, differs from what the controller
/// names, or until [`mj_core::relay::RELAY_CHANGE_WAIT_BOUND`] passes. The
/// state is built as an attach builds it, preparation included, so its digest
/// is the one the controller holds.
pub(crate) async fn wait_for_change(
    relay: &Arc<RelayCell>,
    preparation: Option<&watch::Receiver<PreparationSnapshot>>,
    subagents: Option<&super::subagents::SubagentEndpoint>,
    feed: &super::subagents::SubagentFeed,
    guardian_generation: &mut u64,
    after_ordinal: u64,
    state_digest: Option<&str>,
) -> Result<RelayResponseBody> {
    let mut releases = Some(relay.releases());
    let mut guardian = relay.guardian.changes();
    let mut jev = mj_core::jev::rate_limit_changes();
    let mut preparation = preparation.cloned();
    // A finished preparation closes its channel but keeps its last value,
    // which an attach still reads; keep reading it, stop awaiting it.
    let mut preparation_open = preparation.is_some();
    let mut subagents = subagents.cloned();
    let mut generations = subagents
        .as_ref()
        .map(super::subagents::SubagentEndpoint::generations);
    let deadline = tokio::time::Instant::now() + mj_core::relay::RELAY_CHANGE_WAIT_BOUND;
    loop {
        // Mark each signal seen before looking, so a change while looking
        // still wakes the wait below. Looking only observes the relay, which
        // is not counted as a change.
        if let Some(releases) = &mut releases {
            releases.borrow_and_update();
        }
        let prep_snapshot = preparation
            .as_mut()
            .map(|receiver| receiver.borrow_and_update().clone());
        if subagents.is_none()
            && let Some(endpoint) = prep_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.services.as_ref())
                .and_then(|services| services.subagents.clone())
        {
            generations = Some(endpoint.generations());
            subagents = Some(endpoint);
        }
        if let Some(generations) = &mut generations {
            generations.borrow_and_update();
        }
        jev.borrow_and_update();
        let harness_preparation = prep_snapshot.and_then(|snapshot| snapshot.state);
        let mut state = relay
            .observe()
            .expect("relay state lock poisoned")
            .operational_state();
        state.harness_preparation = harness_preparation;
        let latest_ordinal = state.latest_ordinal;
        let digest = mj_core::relay::relay_state_digest(&state)?;
        let subagent_queue_changed = subagents
            .as_ref()
            .is_some_and(|endpoint| feed.has_unpolled(endpoint));
        let generation = *guardian.borrow_and_update();
        let guardian_queue_changed = generation != *guardian_generation;
        *guardian_generation = generation;
        let changed = guardian_queue_changed
            || latest_ordinal != after_ordinal
            || state_digest != Some(digest.as_str())
            || subagent_queue_changed;
        let observed = RelayResponseBody::Ok {
            payload: RelayResponsePayload::ChangeObserved {
                latest_ordinal,
                state_digest: digest,
                subagent_queue_changed,
                guardian_queue_changed,
            },
        };
        if changed {
            return Ok(observed);
        }
        // A signal whose sender is gone can never change again; stop
        // watching it instead of waking on its closure forever.
        tokio::select! {
            changed = async {
                match &mut releases {
                    Some(receiver) => receiver.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            } => if !changed { releases = None },
            changed = async {
                match &mut generations {
                    Some(receiver) => receiver.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            } => if !changed { generations = None },
            changed = async {
                match preparation.as_mut().filter(|_| preparation_open) {
                    Some(receiver) => receiver.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            } => if !changed { preparation_open = false },
            _ = jev.changed() => {}
            _ = guardian.changed() => {}
            () = tokio::time::sleep_until(deadline) => return Ok(observed),
        }
    }
}

/// Attach replies carry the preparation already; see [`handle_request`].
fn overlay_preparation_state(response: &mut RelayResponseEnvelope, snapshot: &PreparationSnapshot) {
    if let RelayResponseBody::Ok {
        payload: RelayResponsePayload::Status(state),
    } = &mut response.body
    {
        state.harness_preparation = snapshot.state.clone();
    }
}

fn unavailable_request(envelope: RelayRequestEnvelope, message: String) -> RelayResponseEnvelope {
    RelayResponseEnvelope {
        request_id: envelope.request_id,
        protocol_version: envelope.protocol_version,
        body: compaction_error(RelayErrorCode::InvalidState, &message),
    }
}

struct InPlaceHarnessRestartGuard {
    relay: Arc<RelayCell>,
    operation_id: String,
    expected_store_id: String,
    completed: bool,
}

impl InPlaceHarnessRestartGuard {
    fn complete(mut self) -> Result<()> {
        let result = self
            .relay
            .lock()
            .expect("relay state lock poisoned")
            .mark_in_place_harness_restarted(&self.operation_id, &self.expected_store_id);
        if result.is_ok() {
            self.completed = true;
        }
        result
    }
}

impl Drop for InPlaceHarnessRestartGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.relay
                .lock()
                .expect("relay state lock poisoned")
                .cancel_in_place_harness_restart(&self.operation_id);
        }
    }
}

#[derive(Clone, Copy)]
struct ProfileCapabilities {
    managed_subagent: bool,
    subagent_role: Option<mj_core::subagent::SubagentMcpRole>,
    review_tools: bool,
}

pub(crate) struct CommittedReloadAction {
    root: PathBuf,
    capabilities: ProfileCapabilities,
    pending: mj_core::worker_launch::PendingInPlaceReload,
    git_setup: tempfile::TempDir,
    launch_config: Box<WorkerLaunchConfig>,
    disk_launch: Box<WorkerLaunchConfig>,
    environment: BTreeMap<String, String>,
    supervisor_spec: super::AcpSupervisorSpec,
}

#[derive(Debug)]
pub struct AppliedReloadRuntime {
    pub committed: bool,
    pub environment: BTreeMap<String, String>,
    pub subagent_mcp_socket: Option<crate::acp::SubagentMcpSocket>,
    pub managed_subagent: bool,
}

pub struct HarnessReloadTransition {
    pub(crate) commit: Option<CommittedReloadAction>,
    pub(crate) harness: HarnessKind,
    pub(crate) claude_login_path: Option<PathBuf>,
    pub(crate) boundary_mailbox_hook_available: bool,
    pub(crate) mailbox_delivery_mode: Option<mj_core::mailbox::MailboxDeliveryMode>,
    pub(crate) credentials: std::result::Result<CredentialEndpoint, String>,
    pub(crate) shared_credentials: SharedCredentials,
}

impl std::fmt::Debug for HarnessReloadTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessReloadTransition")
            .field("harness", &self.harness)
            .finish_non_exhaustive()
    }
}

type ReloadOutcome =
    std::result::Result<Option<AppliedReloadRuntime>, crate::acp::ReloadResolution>;
type ReloadCompletion = (
    HarnessReloadTransition,
    tokio::sync::oneshot::Sender<ReloadOutcome>,
);

/// Runtime events can be cloned, but only the coordinator consumes this
/// transition and acknowledges it on the event FIFO.
#[derive(Clone, Debug)]
pub struct HarnessReload(Arc<Mutex<Option<ReloadCompletion>>>);

impl HarnessReload {
    pub(crate) fn new(
        transition: HarnessReloadTransition,
    ) -> (Self, tokio::sync::oneshot::Receiver<ReloadOutcome>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Self(Arc::new(Mutex::new(Some((transition, tx))))), rx)
    }

    pub(crate) fn take(self) -> Option<ReloadCompletion> {
        self.0.lock().expect("harness reload lock poisoned").take()
    }
}

/// Credential requests hold a read guard through their filesystem operation.
/// An in-place move drains those requests before swapping the endpoint.
#[derive(Clone)]
pub(crate) struct SharedCredentials(
    Arc<tokio::sync::RwLock<std::result::Result<CredentialEndpoint, String>>>,
);

impl SharedCredentials {
    pub(crate) fn new(initial: std::result::Result<CredentialEndpoint, String>) -> Self {
        Self(Arc::new(tokio::sync::RwLock::new(initial)))
    }

    pub(super) async fn read(
        &self,
    ) -> tokio::sync::RwLockReadGuard<'_, std::result::Result<CredentialEndpoint, String>> {
        self.0.read().await
    }

    pub(super) async fn write(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, std::result::Result<CredentialEndpoint, String>> {
        self.0.write().await
    }

    pub(super) async fn drain(&self) {
        let _guard = self.0.write().await;
    }

    async fn set(&self, updated: std::result::Result<CredentialEndpoint, String>) {
        *self.0.write().await = updated;
    }

    #[cfg(test)]
    pub(super) async fn snapshot(&self) -> std::result::Result<CredentialEndpoint, String> {
        self.read().await.clone()
    }
}

fn background_work_policy_for(harness: HarnessKind) -> crate::relay::BackgroundWorkPolicy {
    use crate::relay::BackgroundWorkPolicy;
    match harness {
        HarnessKind::Claude | HarnessKind::Codex => BackgroundWorkPolicy::AirTasks,
        HarnessKind::Kimi => BackgroundWorkPolicy::KimiTasks,
        HarnessKind::Antigravity => BackgroundWorkPolicy::AntigravityTasks,
        _ => BackgroundWorkPolicy::HostedTerminals,
    }
}

fn harness_turn_policy_for(harness: HarnessKind) -> crate::relay::HarnessTurnPolicy {
    match harness {
        HarnessKind::Claude => crate::relay::HarnessTurnPolicy::ClaudeAdapter,
        HarnessKind::Codex => crate::relay::HarnessTurnPolicy::CodexAdapter,
        HarnessKind::Antigravity => crate::relay::HarnessTurnPolicy::Antigravity,
        _ => crate::relay::HarnessTurnPolicy::Disabled,
    }
}

fn reload_error_response(
    relay: &Arc<RelayCell>,
    operation_id: &str,
    message: String,
) -> RelayResponseBody {
    match dispatch::reload_failure_resolution(relay, message.clone()) {
        Ok(crate::acp::ReloadResolution::FailedAfterCommit(message)) => RelayResponseBody::Error {
            error: mj_core::relay::relay_protocol_error(
                RelayErrorCode::Internal,
                message,
                false,
                Some(mj_core::relay::RelayErrorDetail::ReloadFailedAfterCommit {
                    operation_id: operation_id.to_owned(),
                }),
            ),
        },
        Ok(_) => compaction_error(RelayErrorCode::InvalidState, &message),
        Err(error) => RelayResponseBody::Error {
            error: mj_core::relay::relay_protocol_error(
                RelayErrorCode::Internal,
                format!("{message}; persist reload failure: {error:#}"),
                false,
                Some(mj_core::relay::RelayErrorDetail::ReloadFailedAfterCommit {
                    operation_id: operation_id.to_owned(),
                }),
            ),
        },
    }
}

async fn restart_harness_in_place_response(
    envelope: RelayRequestEnvelope,
    commands: Option<&mpsc::Sender<CommandRequest>>,
    relay: &Arc<RelayCell>,
    credentials: &SharedCredentials,
    relay_root: &std::path::Path,
    shared_subagents: &SharedSubagents,
    preparation_tx: Option<&watch::Sender<PreparationSnapshot>>,
) -> RelayResponseEnvelope {
    // Retain the pre-staging handler contract while old reload callers migrate.
    let legacy_reload = (mj_core::relay::RELAY_IN_PLACE_MOVE_PROTOCOL
        ..mj_core::relay::RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL)
        .contains(&envelope.protocol_version);
    if !legacy_reload
        && let Some(body) = mj_core::relay::protocol::relay_protocol_rejection(&envelope)
    {
        return RelayResponseEnvelope {
            request_id: envelope.request_id,
            protocol_version: envelope.protocol_version,
            body,
        };
    }
    let protocol_version = envelope.protocol_version;
    let request_id = envelope.request_id;
    let RelayRequest::RestartHarnessInPlace {
        candidate_id,
        operation_id,
        expected_store_id,
        launch_config,
    } = envelope.request
    else {
        unreachable!("restart_harness_in_place_response only serves RestartHarnessInPlace")
    };
    let staged = protocol_version >= mj_core::relay::RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL;
    let decision = relay
        .lock()
        .expect("relay state lock poisoned")
        .admit_in_place_harness_restart(&operation_id, &expected_store_id);
    let restart_guard = match decision {
        Err(error) => {
            return RelayResponseEnvelope {
                request_id,
                protocol_version,
                body: RelayResponseBody::Error { error: *error },
            };
        }
        Ok(crate::relay::InPlaceHarnessRestartDecision::AlreadyRestarted) => {
            return RelayResponseEnvelope {
                request_id,
                protocol_version,
                body: RelayResponseBody::Ok {
                    payload: RelayResponsePayload::HarnessRestartedInPlace,
                },
            };
        }
        Ok(crate::relay::InPlaceHarnessRestartDecision::Proceed) => InPlaceHarnessRestartGuard {
            relay: relay.clone(),
            operation_id,
            expected_store_id,
            completed: false,
        },
    };
    let Some(commands) = commands else {
        return RelayResponseEnvelope {
            request_id,
            protocol_version,
            body: compaction_error(
                RelayErrorCode::InvalidState,
                "session is closed; no ACP runtime can restart the harness in place",
            ),
        };
    };
    if staged {
        let validation = candidate_id
            .as_deref()
            .context("staged reload requires candidate_id")
            .and_then(|candidate| {
                super::reload_commit::validate_candidate(
                    relay_root,
                    candidate,
                    &restart_guard.operation_id,
                    &restart_guard.expected_store_id,
                )
            });
        if let Err(error) = validation {
            return RelayResponseEnvelope {
                request_id,
                protocol_version,
                body: compaction_error(RelayErrorCode::InvalidState, &format!("{error:#}")),
            };
        }
    }
    let writer = match PreparationWriter::try_acquire(
        &relay.preparation_writer,
        format!("in-place harness restart {}", restart_guard.operation_id),
    ) {
        Ok(writer) => writer,
        Err(error) => {
            return RelayResponseEnvelope {
                request_id,
                protocol_version,
                body: RelayResponseBody::Error { error: *error },
            };
        }
    };
    let launch_config = if staged {
        let path = relay_root
            .join(mj_core::worker_launch::RELOAD_CANDIDATES_DIR)
            .join(candidate_id.as_ref().expect("validated candidate"))
            .join("launch.json");
        match WorkerLaunchConfig::read(&path) {
            Ok(launch) => Box::new(launch),
            Err(error) => {
                return RelayResponseEnvelope {
                    request_id,
                    protocol_version,
                    body: compaction_error(
                        RelayErrorCode::InvalidState,
                        &format!("read candidate launch: {error:#}"),
                    ),
                };
            }
        }
    } else {
        launch_config
    };
    let disk_launch = launch_config.clone();
    let mut launch_config = launch_config;

    let initialize = std::env::current_dir()
        .context("resolve worker startup directory")
        .and_then(|startup_directory| {
            initialize_launch_environment(&mut launch_config, &startup_directory);
            super::enforce_execution_policy(&mut launch_config)
                .context("enforce execution policy for in-place harness restart")
        });
    if let Err(error) = initialize {
        return RelayResponseEnvelope {
            request_id,
            protocol_version,
            body: compaction_error(RelayErrorCode::InvalidState, &format!("{error:#}")),
        };
    }
    let cancel = CancellationToken::new();
    let prepared = writer
        .track(
            &cancel,
            prepare_harness_private(relay_root, &mut launch_config, relay, None, &cancel, false),
        )
        .await;
    let PreparedHarnessEnvironment {
        launch: prepared,
        git_setup,
        boundary_mailbox_hook_available,
        mailbox_delivery_mode: updated_mailbox_delivery_mode,
        profile_registration,
        managed_subagent,
        subagent_role,
        // Project tools belong to the retained environment's startup lifecycle,
        // rather than to each bridge reload.
        project_tools_install: _,
    } = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            return RelayResponseEnvelope {
                request_id,
                protocol_version,
                body: compaction_error(
                    RelayErrorCode::InvalidState,
                    &format!("prepare updated harness launch: {error:#}"),
                ),
            };
        }
    };
    if subagent_role.is_some()
        && let Err(error) = shared_subagents.ensure_serving(
            &tokio::runtime::Handle::current(),
            relay_root,
            relay.clone(),
            launch_config.harness,
            launch_config.subagent_kind,
            preparation_tx,
        )
    {
        return RelayResponseEnvelope {
            request_id,
            protocol_version,
            body: compaction_error(
                RelayErrorCode::InvalidState,
                &format!("start sub-agent socket: {error:#}"),
            ),
        };
    }
    let supervisor_path = relay_root.join("acp-supervisor.json");
    let additional_directories =
        super::acp_session_directories(&launch_config, &prepared.environment);
    let updated_credentials = super::credential_endpoint(&launch_config);
    let updated_harness = launch_config.harness;

    let transition = HarnessReloadTransition {
        commit: Some(CommittedReloadAction {
            root: relay_root.to_owned(),
            capabilities: ProfileCapabilities {
                managed_subagent,
                subagent_role,
                review_tools: launch_config.review_tools(),
            },
            pending: mj_core::worker_launch::PendingInPlaceReload {
                operation_id: restart_guard.operation_id.clone(),
                expected_store_id: restart_guard.expected_store_id.clone(),
                candidate_id: candidate_id.clone().unwrap_or_default(),
                git_setup_name: None,
            },
            git_setup,
            launch_config: launch_config.clone(),
            disk_launch,
            environment: prepared.environment.clone(),
            supervisor_spec: prepared.spec.clone(),
        }),
        harness: updated_harness,
        claude_login_path: (updated_harness == HarnessKind::Claude)
            .then(|| mj_core::credentials::claude_oauth_login_path(&launch_config.environment))
            .flatten(),
        boundary_mailbox_hook_available,
        mailbox_delivery_mode: updated_mailbox_delivery_mode,
        credentials: updated_credentials.clone(),
        shared_credentials: credentials.clone(),
    };
    writer.gate.set_step("reload harness", chrono::Utc::now());
    let (resolved, resolution) = tokio::sync::oneshot::channel();
    let body = match commands
        .send(CommandRequest::ReloadLaunchConfig {
            subagent_mcp_socket: crate::acp::launch_subagent_mcp_socket(
                relay_root,
                &launch_config,
                subagent_role,
                profile_registration,
            ),
            managed_subagent,
            launch_config,
            environment: prepared.environment,
            command: None,
            args: None,
            additional_directories,
            supervisor_spec: Some(Box::new((supervisor_path, prepared.spec))),
            leases: Some(Box::new((prepared.managed, prepared.managed_node))),
            on_restart: Some(Box::new(transition)),
            resolved,
        })
        .await
    {
        Ok(()) => match resolution.await {
            Ok(crate::acp::ReloadResolution::Started) => {
                if let Err(error) = restart_guard.complete() {
                    compaction_error(
                        RelayErrorCode::Internal,
                        &format!("persist in-place harness restart state: {error:#}"),
                    )
                } else {
                    RelayResponseBody::Ok {
                        payload: RelayResponsePayload::HarnessRestartedInPlace,
                    }
                }
            }
            Ok(
                crate::acp::ReloadResolution::RefusedBeforeCommit(message)
                | crate::acp::ReloadResolution::FailedAfterCommit(message),
            ) => reload_error_response(relay, &restart_guard.operation_id, message),
            Err(_) => reload_error_response(
                relay,
                &restart_guard.operation_id,
                "ACP runtime stopped before completing in-place harness restart".into(),
            ),
        },
        Err(_) => reload_error_response(
            relay,
            &restart_guard.operation_id,
            "ACP runtime is unavailable for in-place harness restart".into(),
        ),
    };
    RelayResponseEnvelope {
        request_id,
        protocol_version,
        body,
    }
}

#[derive(Clone)]
pub(super) struct ProjectMemoryEndpoint {
    config: Option<super::ProjectMemoryLaunchConfig>,
    pub(super) io: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    pub(super) io_requested: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    pub(super) io_disconnected: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    pub(super) io_block: Option<Arc<ProjectMemoryIoBlock>>,
    history: super::history::HistoryEndpoint,
    cwd: PathBuf,
}

#[cfg(test)]
pub(super) struct ProjectMemoryIoBlock {
    pub(super) started: tokio::sync::Notify,
    pub(super) release: std::sync::Mutex<std::sync::mpsc::Receiver<Result<()>>>,
    pub(super) panic_after_release: bool,
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use mj_core::history::{HistoryQuery, HistoryResult};
    use mj_core::relay::RELAY_PROTOCOL_VERSION;

    async fn send(connection: &mut BufReader<UnixStream>, request: RelayRequest) {
        let mut bytes = serde_json::to_vec(&RelayRequestEnvelope {
            request_id: "request".into(),
            protocol_version: RELAY_PROTOCOL_VERSION,
            request,
        })
        .unwrap();
        bytes.push(b'\n');
        connection.get_mut().write_all(&bytes).await.unwrap();
    }
    async fn receive(connection: &mut BufReader<UnixStream>) -> RelayResponsePayload {
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connection.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let response: RelayResponseEnvelope = serde_json::from_str(&line).unwrap();
        match response.body {
            RelayResponseBody::Ok { payload } => payload,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn history_round_trips_large_replies_without_delegation_or_durable_events() {
        let temp = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            DurableRelay::open(temp.path(), "history-test", "1.0.0").unwrap(),
        ));
        let memory = ProjectMemoryEndpoint::new(Some(super::super::ProjectMemoryLaunchConfig {
            project_key: "key".into(),
            root: temp.path().join("memory"),
            baseline_root: temp.path().join("baseline"),
            repository_roots: Default::default(),
            mcp_delivery: mj_core::worker_launch::ProjectMemoryMcpDelivery::Acp,
            history_socket: Some(temp.path().join("control.sock")),
        }));
        let (wake, _received) = mpsc::channel(1);
        let (fatal, _errors) = mpsc::channel(1);
        let mut tasks = Vec::new();
        let mut connections = Vec::new();
        for _ in 0..2 {
            let (server, client) = UnixStream::pair().unwrap();
            tasks.push(mj_core::runtime::spawn(serve_client_with_memory(
                server,
                relay.clone(),
                wake.clone(),
                Err("no credentials".into()),
                ConnectionRuntime {
                    project_memory: memory.clone(),
                    ..Default::default()
                },
                fatal.clone(),
            )));
            connections.push(BufReader::new(client));
        }
        send(
            &mut connections[0],
            RelayRequest::HistoryQuery {
                query: HistoryQuery::SearchSessions {
                    query: "needle".into(),
                    limit: 20,
                },
            },
        )
        .await;
        let requests = loop {
            send(&mut connections[1], RelayRequest::HistoryRequests).await;
            let RelayResponsePayload::HistoryRequests { requests } =
                receive(&mut connections[1]).await
            else {
                panic!("history requests");
            };
            if !requests.is_empty() {
                break requests;
            }
            tokio::task::yield_now().await;
        };
        let value = serde_json::json!({"text":"é🙂".repeat(30_000)});
        send(
            &mut connections[1],
            RelayRequest::CompleteHistoryRequest {
                result: HistoryResult {
                    request_id: requests[0].request_id.clone(),
                    value: value.clone(),
                    is_error: false,
                },
            },
        )
        .await;
        assert!(matches!(
            receive(&mut connections[1]).await,
            RelayResponsePayload::HistoryRequestCompleted
        ));
        let RelayResponsePayload::HistoryResult { result } = receive(&mut connections[0]).await
        else {
            panic!("history result");
        };
        assert_eq!(result.value, value);
        // Disconnect while another request is waiting for the controller.
        send(
            &mut connections[0],
            RelayRequest::HistoryQuery {
                query: HistoryQuery::SearchSessions {
                    query: "a".into(),
                    limit: 20,
                },
            },
        )
        .await;
        while memory.history.requests().is_empty() {
            tokio::task::yield_now().await;
        }
        drop(connections);
        for task in tasks {
            tokio::time::timeout(std::time::Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert!(memory.history.requests().is_empty());
        assert_eq!(
            relay.lock().unwrap().operational_state().session_id,
            "history-test"
        );
    }
    #[tokio::test]
    async fn guardian_relay_wakes_and_preserves_pending_requests_across_disconnect() {
        use mj_core::guardian::{
            GuardianReviewOutcome, GuardianReviewRequest, GuardianReviewResult, RiskLevel,
            UserAuthorization,
        };
        let temp = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            DurableRelay::open(temp.path(), "guardian-test", "1.0.0").unwrap(),
        ));
        let (wake, _received) = mpsc::channel(1);
        let (fatal, _errors) = mpsc::channel(1);
        let connect = || {
            let (server, client) = UnixStream::pair().unwrap();
            let task = mj_core::runtime::spawn(serve_client_with_memory(
                server,
                Arc::clone(&relay),
                wake.clone(),
                Err("no credentials".into()),
                ConnectionRuntime::default(),
                fatal.clone(),
            ));
            (BufReader::new(client), task)
        };
        let (mut connection, server) = connect();
        let state = relay.observe().unwrap().operational_state();
        send(
            &mut connection,
            RelayRequest::WaitForChange {
                after_ordinal: state.latest_ordinal,
                state_digest: Some(mj_core::relay::relay_state_digest(&state).unwrap()),
            },
        )
        .await;

        // Exercise a real reply larger than a pipe buffer, and all four
        // decisions the daemon can make.
        let request = GuardianReviewRequest {
            request_id: String::new(),
            harness: HarnessKind::Claude,
            session_id: "guardian-test".into(),
            tool_call_id: "call".into(),
            action: serde_json::json!({"input": "x".repeat(70_000)}),
            user_instructions: vec![],
            user_instructions_omitted: false,
            project_instructions: None,
            trusted_user_inputs: vec![mj_core::guardian::TrustedUserInput {
                id: "ask".into(),
                kind: mj_core::guardian::TrustedUserInputKind::UserAnswer,
                title: None,
                prompt: "Proceed?".into(),
                action: "accept".into(),
                answers: Vec::new(),
                legacy_reply: Some("Yes".into()),
            }],
            parent_instructions: Vec::new(),
            parent_instructions_omitted: false,
            prior_reviews: vec![mj_core::guardian::PriorGuardianReview {
                review_id: None,
                tool: "shell".into(),
                action_summary: "make deploy".into(),
                outcome: mj_core::guardian::Outcome::Allow,
                risk_level: None,
                user_authorization: None,
                rationale: None,
            }],
            retry_reason: Some("New reply".into()),
            transcript: String::new(),
        };
        let pending = || {
            let endpoint = relay.guardian.clone();
            let request = request.clone();
            mj_core::runtime::spawn(async move {
                endpoint
                    .review(
                        request,
                        tokio::time::Instant::now() + mj_core::guardian::GUARDIAN_REVIEW_TIMEOUT,
                    )
                    .await
                    .unwrap()
            })
        };
        let first = pending();
        assert!(matches!(
            receive(&mut connection).await,
            RelayResponsePayload::ChangeObserved {
                guardian_queue_changed: true,
                ..
            },
        ));
        send(&mut connection, RelayRequest::GuardianRequests).await;
        let RelayResponsePayload::GuardianRequests { requests } = receive(&mut connection).await
        else {
            panic!("guardian requests")
        };
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].action, request.action);
        let request_id = requests[0].request_id.clone();
        assert_eq!(requests[0].trusted_user_inputs, request.trusted_user_inputs);
        assert_eq!(requests[0].prior_reviews, request.prior_reviews);
        assert_eq!(requests[0].retry_reason, request.retry_reason);
        drop(connection);
        server.await.unwrap().unwrap();
        assert!(!first.is_finished());

        let (mut replacement, server) = connect();
        let decisions = [
            GuardianReviewOutcome::Allow {
                stage: "utility_llm".into(),
                stage1_score: None,
            },
            GuardianReviewOutcome::Deny {
                rationale: "risk".into(),
                risk_level: RiskLevel::High,
                user_authorization: UserAuthorization::Low,
                stage: "utility_llm".into(),
                stage1_score: None,
            },
            GuardianReviewOutcome::UseAcpFallback { stage1_score: None },
            GuardianReviewOutcome::AskPerson {
                reason: "missing evidence".into(),
            },
        ];
        let mut first = Some(first);
        for decision in decisions {
            let reconnecting = first.is_some();
            let task = first.take().unwrap_or_else(pending);
            let requests = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    send(&mut replacement, RelayRequest::GuardianRequests).await;
                    let RelayResponsePayload::GuardianRequests { requests } =
                        receive(&mut replacement).await
                    else {
                        panic!("guardian requests")
                    };
                    if !requests.is_empty() {
                        break requests;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(requests.len(), 1);
            let pending_id = requests[0].request_id.clone();
            if reconnecting {
                assert_eq!(request_id, pending_id);
            }
            let result = GuardianReviewResult {
                request_id: pending_id,
                decision,
            };
            send(
                &mut replacement,
                RelayRequest::CompleteGuardianRequest {
                    result: result.clone(),
                },
            )
            .await;
            assert!(matches!(
                receive(&mut replacement).await,
                RelayResponsePayload::GuardianRequestCompleted
            ));
            assert_eq!(task.await.unwrap(), result);
        }
        drop(replacement);
        server.await.unwrap().unwrap();
        assert!(relay.guardian.requests().is_empty());
        assert_eq!(
            relay.observe().unwrap().operational_state().latest_ordinal,
            state.latest_ordinal
        );
    }
}

impl ProjectMemoryEndpoint {
    pub(super) fn new(config: Option<super::ProjectMemoryLaunchConfig>) -> Self {
        Self {
            config,
            io: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)]
            io_requested: Arc::new(tokio::sync::Notify::new()),
            #[cfg(test)]
            io_disconnected: Arc::new(tokio::sync::Notify::new()),
            #[cfg(test)]
            io_block: None,
            history: Default::default(),
            cwd: PathBuf::new(),
        }
    }
}

impl Default for ProjectMemoryEndpoint {
    fn default() -> Self {
        Self::new(None)
    }
}

pub(super) struct SocketGuard(pub(super) PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        remove_control_socket(&self.0, "shutdown").unwrap_or_else(|error| {
            tracing::warn!(%error, socket = %self.0.display(), "control socket shutdown cleanup failed");
        });
    }
}

// Capture both sides: overlay filesystems can report a successful operation
// while subsequent path lookup still exposes an old inode.
fn log_control_socket(path: &std::path::Path, operation: &str, error: Option<&anyhow::Error>) {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path);
    let errno = error
        .and_then(|error| error.downcast_ref::<std::io::Error>())
        .and_then(std::io::Error::raw_os_error);
    let inode = metadata.as_ref().ok().map(MetadataExt::ino);
    let links = metadata.as_ref().ok().map(MetadataExt::nlink);
    let mode = metadata.as_ref().ok().map(MetadataExt::mode);
    let device = metadata.as_ref().ok().map(MetadataExt::dev);
    let file_type = metadata.as_ref().ok().map(|m| m.file_type());
    let lookup_errno = metadata
        .as_ref()
        .err()
        .and_then(std::io::Error::raw_os_error);
    tracing::info!(socket = %path.display(), operation, success = error.is_none(),
        ?errno, ?error, ?inode, ?links, ?mode, ?device, ?file_type, ?lookup_errno,
        pid = std::process::id(), "control socket operation");
}

fn remove_control_socket(path: &std::path::Path, phase: &str) -> Result<()> {
    log_control_socket(path, &format!("{phase}: before unlink"), None);
    let result = std::fs::remove_file(path).map_err(anyhow::Error::from);
    log_control_socket(path, &format!("{phase}: unlink"), result.as_ref().err());
    result
}

/// Bind `control.sock`, restrict it to the owner, and record the `bind-socket`
/// and `serving` startup steps. The caller must accept on the listener
/// promptly: the daemon reads the socket's existence as "this worker answers".
fn publish_control_socket(
    root: &std::path::Path,
    socket: &std::path::Path,
) -> Result<(UnixListener, SocketGuard)> {
    super::record_startup_step(root, "bind-socket");
    log_control_socket(socket, "before bind", None);
    let bound = bind_unix_listener(socket);
    log_control_socket(socket, "bind", bound.as_ref().err());
    let listener = bound.with_context(|| format!("bind worker socket {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set worker socket {} nonblocking", socket.display()))?;
    let listener = UnixListener::from_std(listener)
        .with_context(|| format!("register worker socket {}", socket.display()))?;
    let guard = SocketGuard(socket.to_owned());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    }
    super::record_startup_step(root, "serving");
    Ok((listener, guard))
}

use super::WORKER_PID_FILE;

/// Record this daemon's PID where session teardown can find it. Teardown
/// must stop the daemon before deleting the worker root; without this file
/// it can only guess from process command lines.
pub(super) fn write_worker_pidfile(root: &std::path::Path, pid: u32) -> Result<()> {
    let path = root.join(WORKER_PID_FILE);
    std::fs::write(&path, format!("{pid}\n"))
        .with_context(|| format!("write worker pidfile {}", path.display()))
}

pub async fn run_daemon(root: PathBuf, config: WorkerLaunchConfig) -> Result<()> {
    let owner = super::WorkerRootOwner::acquire(&root)?;
    run_daemon_owned(&owner, config).await
}

pub async fn run_daemon_owned(
    owner: &super::WorkerRootOwner,
    mut config: WorkerLaunchConfig,
) -> Result<()> {
    let root = owner.root().to_owned();
    config
        .target_environment
        .remove(mj_core::worker_launch::SESSION_NITRO_MBX_ENV);
    if let Some(key) = config
        .target_environment
        .remove(mj_core::jev::FASTINO_KEY_ENVIRONMENT)
    {
        anyhow::ensure!(
            crate::acp::verdict_client::FASTINO_KEY.set(key).is_ok(),
            "the worker's Fastino key was already initialized"
        );
    }
    let startup_directory = std::env::current_dir()?;
    let root = super::resolve_relative_worker_root(root, &startup_directory);
    initialize_launch_environment(&mut config, &startup_directory);
    let checkpoint_only = config.run_mode == mj_core::worker_launch::WorkerRunMode::CheckpointOnly;
    super::record_startup_step(&root, "policy");
    if !checkpoint_only {
        super::enforce_execution_policy(&mut config)?;
    }
    // Resolve this before the launch config's environment is consumed by
    // the ACP supervisor specification below.
    let credentials = super::credential_endpoint(&config);
    mj_core::config::create_owner_only_directory(&root)
        .with_context(|| format!("create worker root {}", root.display()))?;
    let socket = root.join("control.sock");
    // Refuse a second daemon before touching durable state: opening the
    // relay recovers the journal in place, so getting that far would
    // corrupt the files a live worker is still writing.
    let exists = socket.exists();
    log_control_socket(
        &socket,
        if exists {
            "startup: exists"
        } else {
            "startup: absent"
        },
        None,
    );
    if exists {
        let connected = connect_unix_stream(&socket);
        log_control_socket(&socket, "startup: connect", connected.as_ref().err());
        if connected.is_ok() {
            bail!("a worker is already running at {}", socket.display());
        }
    }
    // A dead daemon can leave its socket inode behind. Remove it before
    // journal recovery so controller liveness can distinguish a worker
    // that is still starting from one that has published its endpoint.
    if socket.exists() {
        remove_control_socket(&socket, "startup")
            .with_context(|| format!("remove stale socket {}", socket.display()))?;
    }
    // Check the target directory before serving any relay requests. Otherwise
    // a child process can fail to spawn with ENOENT for its cwd, and a caller
    // that already attached can see only the ensuing relay disconnect.
    if !checkpoint_only {
        const STEP: &str = "working-directory";
        super::record_startup_step(&root, STEP);
        // Only a directory that is gone or is not a directory is permanent.
        // Other errors, such as ESTALE or EIO from a network filesystem, can
        // clear up. They exit with an ordinary error and no startup-failure
        // marker, because every reader of that marker (unpark and recovery)
        // takes it to mean that starting again would only repeat the failure.
        let problem = match std::fs::metadata(&config.cwd) {
            Ok(metadata) if metadata.is_dir() => None,
            Ok(_) => Some(("path is not a directory".to_owned(), true)),
            Err(error) => {
                let permanent = matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                );
                Some((error.to_string(), permanent))
            }
        };
        if let Some((problem, permanent)) = problem {
            let message = format!(
                "worker working directory {} is unavailable: {problem}",
                config.cwd.display()
            );
            super::record_startup_failure(&root, STEP, &message);
            if !permanent {
                return Err(anyhow::anyhow!(message));
            }
            return Err(mj_core::relay::WorkerHarnessStartupFailure {
                message,
                kind: Some(
                    mj_core::relay::WorkerHarnessStartupFailureKind::WorkingDirectoryUnavailable,
                ),
            }
            .into());
        }
    }
    let relay_state_exists = root.join(RELAY_STATE_FILE).exists();
    let restarting = relay_state_exists || root.join(RESTORED_RELAY_SEED_FILE).exists();
    // Validate and recover durable state before publishing a socket. A
    // failed startup must never leave a fresh endpoint that looks live.
    super::record_startup_step(&root, "durable-relay");
    let mut durable_relay = if checkpoint_only {
        DurableRelay::open_for_checkpoint_with_initial_commands(
            &root,
            &config.session_id,
            env!("CARGO_PKG_VERSION"),
            &config.initial_commands,
        )?
    } else {
        DurableRelay::open_for_harness(
            &root,
            &config.session_id,
            env!("CARGO_PKG_VERSION"),
            config.harness,
            &config.initial_commands,
        )?
    };
    if let Some(operation_id) = config.accepted_reload_operation_id.as_deref() {
        durable_relay.reconcile_committed_reload(operation_id)?;
    }
    // A sealed relay only finishes its accepted close. An unsealed capture
    // must drain writers before it can admit a checkpoint barrier.
    if checkpoint_only
        && !durable_relay.sealed_by_close()
        && let Some(capture) = config.suspend_capture.take()
    {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            super::record_startup_step(&root, "checkout-quiescence");
            let root = root.clone();
            let session_id = config.session_id.clone();
            mj_core::runtime::spawn_blocking(move || {
                crate::worker_stop::suspension::terminate_session_processes(
                    &root,
                    &session_id,
                    &capture,
                    false,
                )
            })
            .await
            .context("join checkpoint checkout quiescence")??;
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            drop(capture);
            return Err(mj_core::refusal::Refusal::precondition(
                "Managed checkout quiescence requires Linux or macOS",
            )
            .into());
        }
    }
    // Only Claude Code's adapter marks the end of a turn it started on its
    // own, so only it can model those turns without leaving a session stuck
    // Running. See `.agents/docs/claude-autonomous-turns.md`.
    durable_relay.jev_disabled |= config
        .environment
        .get(mj_core::jev::DISABLED_ENVIRONMENT)
        .is_some_and(|value| value == "1");
    durable_relay.recovers_child_quota = config.subagent_kind
        == Some(mj_core::subagent::SubagentKind::Worker)
        && !durable_relay.jev_disabled;
    durable_relay.set_managed_subagent(config.is_managed_subagent())?;
    durable_relay.set_turn_verdict_harness(config.harness);
    if config.harness == HarnessKind::Claude {
        durable_relay.authentication_login_path =
            mj_core::credentials::claude_oauth_login_path(&config.environment);
    }
    // Worker admission opens before profile preparation finishes. Claude must
    // already be protected from ACP steering during that window.
    durable_relay.set_steering_delivery_mode(config.harness, false);
    durable_relay.set_harness_turn_policy(harness_turn_policy_for(config.harness));
    durable_relay.set_background_work_policy(background_work_policy_for(config.harness));
    record_imported_native_identity(&config, &mut durable_relay)?;
    let resume_session = select_resume_session(&config, &durable_relay);
    let native_session_history = durable_relay.native_session_history();
    let mut project_memory = ProjectMemoryEndpoint::new(config.project_memory.clone());
    project_memory.cwd = config.cwd.clone();
    // Startup succeeded far enough to own this root, so claim it. A failed
    // open leaves any previous pidfile alone rather than pointing teardown
    // at a process that never took over.
    write_worker_pidfile(&root, std::process::id())?;
    // Durable state recovered, so any exit record belongs to a previous
    // life of this worker. Leaving it would make the controller read this
    // startup as another death. Replacing it with a reservation also keeps
    // room for this life's record: a worker that stops on a full disk can
    // still say so.
    if let Err(error) = crate::exit_record::reserve(&root) {
        mj_core::diagnostic_stderr!("Mjolnir: could not reserve worker exit record: {error:#}");
    }

    if restarting
        && durable_relay.operational_state().execution
            != mj_core::relay::RelayExecutionState::Closed
    {
        durable_relay.record_observation(RelayObservation::SessionRestarted)?;
    }

    // Only a managed sub-agent that is not a reviewer observes its agent's work
    // when a turn ends (`SESSION_MANAGED_SUBAGENT_ENV`, set by the controller for
    // every sub-agent, and no `reviewer_launch`): an ordinary session has no
    // parent to report to, and a reviewer's verdict names no working tree.
    let capture = (config.is_managed_subagent() && config.reviewer_launch.is_none()).then(|| {
        crate::work_facts::WorkObserver {
            session_id: config.session_id.clone(),
            worker_pid: std::process::id(),
            directory: config.cwd.clone(),
            git: "git".into(),
            harness_ready_ms: durable_relay.harness_ready_clock(),
            children: Default::default(),
        }
    });
    let relay = Arc::new(RelayCell::new(durable_relay));
    if let Ok(endpoint) = &credentials
        && endpoint.harness != HarnessKind::Claude
    {
        let endpoint = endpoint.clone();
        let relay = relay.clone();
        mj_core::runtime::spawn(async move {
            let identity = relay
                .observe()
                .expect("relay state lock poisoned")
                .login_identity
                .clone();
            if let Err(error) = requests::observe_initial_login(&endpoint, &identity).await {
                tracing::warn!(%error, "could not read the session's staged login");
            }
            // Make the new observation visible to a relay change waiter.
            drop(relay.lock().expect("relay state lock poisoned"));
        });
    }
    // Client tasks are detached, so a durable failure they cannot recover
    // from has to travel back here to stop the daemon.
    let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
    // A managed child's worker keeps `work_liveness` true while the processes
    // its last turn left running exist. The watcher resumes from the snapshot
    // after a restart and only ends on a durable relay failure.
    if let Some(observer) = capture.clone() {
        let relay = relay.clone();
        let fatal = fatal_tx.clone();
        mj_core::runtime::spawn(async move {
            let error = crate::work_facts::watch_liveness(
                relay,
                observer,
                crate::work_facts::LIVENESS_INTERVAL,
            )
            .await;
            let _ = fatal.send(error).await;
        });
    }
    let (cpu_tx, cpu_rx) = tokio::sync::watch::channel(Ok(None));
    let cpu_task = mj_core::runtime::spawn(crate::cpu_usage::sample_cpu(
        cpu_tx,
        config.cpu_accounting.clone(),
    ));
    struct StopCpu(tokio::task::AbortHandle);
    impl Drop for StopCpu {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop_cpu = StopCpu(cpu_task.abort_handle());
    let cpu_fatal = fatal_tx.clone();
    let cpu_session_id = config.session_id.clone();
    mj_core::runtime::spawn(async move {
        match cpu_task.await {
            Err(error) if error.is_cancelled() => {}
            result => {
                tracing::error!(session_id = %cpu_session_id, ?result, "worker CPU sampler stopped");
                let _ = cpu_fatal
                    .send(anyhow::anyhow!("worker CPU sampler stopped: {result:?}"))
                    .await;
            }
        }
    });
    if checkpoint_only
        || relay
            .lock()
            .expect("relay state lock poisoned")
            .operational_state()
            .execution
            == mj_core::relay::RelayExecutionState::Closed
    {
        // A durable close is a seal, including across target or daemon
        // restarts. Keep the relay attachable so the controller can catch
        // up and complete its checkpoint, but never reopen the ACP session.
        if checkpoint_only {
            relay
                .lock()
                .expect("relay state lock poisoned")
                .dispatch_checkpoint_only()?;
        }
        let (dispatch_wake_tx, dispatch_wake_rx) = mpsc::channel(1);
        drop(dispatch_wake_rx);
        // Nothing here waits on a harness, so the relay can answer at once.
        let (listener, _socket_guard) = publish_control_socket(&root, &socket)?;
        return serve_terminal_relay(
            listener,
            relay,
            dispatch_wake_tx,
            credentials,
            ConnectionRuntime {
                project_memory,
                cpu: Some(cpu_rx),
                ..Default::default()
            },
            fatal_tx,
            fatal_rx,
        )
        .await;
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let mut suspend_signal = crate::worker_stop::suspension::SuspendSignal::install(&root)?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let mut suspend_signal = ();
    let mut suspend_request = None;
    let suspend_session_id = config.session_id.clone();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use crate::worker_stop::suspension::{finish as finish_suspend, wait as wait_for_suspend};
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    async fn wait_for_suspend(_: &mut ()) -> Result<mj_core::worker_suspend::SuspendRequest> {
        std::future::pending().await
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    async fn finish_suspend(
        _: PathBuf,
        _: String,
        _: mj_core::worker_suspend::SuspendRequest,
    ) -> Result<()> {
        unreachable!("unsupported suspend signal is always pending")
    }

    let shared_subagents = SharedSubagents::default();
    let initial_step = "login-resolve";
    let (preparation_tx, preparation_rx) = watch::channel(PreparationSnapshot {
        state: Some(mj_core::relay::HarnessPreparation::Preparing {
            step: initial_step.to_owned(),
            since_ms: chrono::Utc::now().timestamp_millis(),
        }),
        services: None,
        step_deadline: None,
    });
    let preparation_cancel = CancellationToken::new();
    let (dispatch_wake_tx, dispatch_wake_rx) = mpsc::channel(1);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install worker shutdown signal handler")?;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // The adoption regression loses only its controller before this worker
    // accepts any ACK, so the ordinary genesis path is exercised end to end.
    mj_core::test_hooks::reach_test_hook("adoption_orphan_before_serving")?;
    // The relay is recovered; accept immediately while harness work runs
    // under its own supervisor.
    let (listener, socket_guard) = publish_control_socket(&root, &socket)?;
    let subagent_role = config.subagent_mcp_role();
    let has_subagent_tools = subagent_role.is_some();
    let (antigravity_refresh_tx, antigravity_refresh_rx) = mpsc::channel(16);
    let task_monitors = TaskMonitors::new(config.harness, &credentials);
    let shared_credentials = SharedCredentials::new(credentials);
    let mut client_task = mj_core::runtime::spawn(accept_worker_clients(
        listener,
        socket_guard,
        relay.clone(),
        dispatch_wake_tx.clone(),
        shared_credentials,
        ConnectionRuntime {
            antigravity_refresh: Some(antigravity_refresh_tx),
            project_memory,
            cpu: Some(cpu_rx),
            shared_subagents: shared_subagents.clone(),
            preparation_tx: Some(preparation_tx.clone()),
            preparation: Some(preparation_rx),
            preparation_cancel: Some(preparation_cancel.clone()),
            has_subagent_tools,
            ..Default::default()
        },
        fatal_tx.clone(),
    ));
    let _abort_clients_on_drop = AbortTaskOnDrop(client_task.abort_handle());

    let preparation_setup = HarnessPreparationSetup {
        shared_subagents: shared_subagents.clone(),
        antigravity_refresh: antigravity_refresh_rx,
        root: root.clone(),
        config,
        relay: relay.clone(),
        restarting,
        task_monitors,
        resume_session,
        native_session_history,
        capture,
    };
    let prep_status = preparation_tx;
    let prep_cancel = preparation_cancel.clone();
    let mut prep_task = mj_core::runtime::spawn(async move {
        supervise_harness_preparation(
            preparation_setup,
            dispatch_wake_rx,
            prep_status,
            prep_cancel,
        )
        .await
    });
    let _cancel_preparation_on_drop = CancelPreparationOnDrop(preparation_cancel.clone());

    // A Close during preparation cancels it from the request handler; the
    // supervisor then returns `Cancelled` once its threads have returned.
    let preparation_outcome = tokio::select! {
        biased;
        result = &mut prep_task => result,
        request = wait_for_suspend(&mut suspend_signal) => {
            let request = request?;
            stop_preparation(&preparation_cancel, &mut prep_task).await;
            client_task.abort();
            let _ = client_task.await;
            return finish_suspend(root, suspend_session_id, request).await;
        }
        fatal = fatal_rx.recv() => {
            stop_preparation(&preparation_cancel, &mut prep_task).await;
            client_task.abort();
            return Err(fatal.unwrap_or_else(|| anyhow::anyhow!("relay failure report was lost"))
                .context("relay durable state became unwritable"));
        }
        accepted = &mut client_task => {
            stop_preparation(&preparation_cancel, &mut prep_task).await;
            let error = accepted
                .context("worker proxy accept task stopped")?
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("worker proxy accept task stopped"));
            return Err(error);
        }
        _ = &mut ctrl_c => {
            stop_preparation(&preparation_cancel, &mut prep_task).await;
            client_task.abort();
            return Ok(());
        }
        _ = terminate.recv() => {
            stop_preparation(&preparation_cancel, &mut prep_task).await;
            client_task.abort();
            return Ok(());
        }
    };

    let running_harness = match preparation_outcome {
        Ok(PreparationOutcome::Started(running)) => *running,
        Ok(PreparationOutcome::Failed(error)) => {
            let mut durable = relay.lock().expect("relay lock");
            if let Some((operation_id, store_id)) = durable.committed_reload_operation()
                && let Err(persist_error) = durable.mark_in_place_harness_failed_after_commit(
                    &operation_id,
                    &store_id,
                    format!("{error:#}"),
                )
            {
                tracing::warn!(%persist_error, "could not persist committed reload preparation failure");
            }
            drop(durable);
            // The exit record carries the typed failure, so the daemon stops
            // restarting this harness and reports why it could not start.
            client_task.abort();
            return Err(error);
        }
        Ok(PreparationOutcome::Cancelled(dispatch_wakes)) => {
            // A Close sealed the relay before the bridge started. Serve it
            // without a harness so the controller can finish its checkpoint.
            // The wake receiver stays open: a request that releases the
            // close's barrier still wakes dispatch, and the handler
            // completes the Close locally.
            let _dispatch_wakes = dispatch_wakes;
            tokio::select! {
                request = wait_for_suspend(&mut suspend_signal) => {
                    client_task.abort();
                    let _ = client_task.await;
                    return finish_suspend(root, suspend_session_id, request?).await;
                }
                fatal = fatal_rx.recv() => {
                    client_task.abort();
                    return Err(fatal.unwrap_or_else(|| anyhow::anyhow!("relay failure report was lost"))
                        .context("relay durable state became unwritable"));
                }
                accepted = &mut client_task => {
                    let error = accepted
                        .context("worker proxy accept task stopped")?
                        .err()
                        .unwrap_or_else(|| anyhow::anyhow!("worker proxy accept task stopped"));
                    return Err(error);
                }
                _ = &mut ctrl_c => {
                    client_task.abort();
                    return Ok(());
                }
                _ = terminate.recv() => {
                    client_task.abort();
                    return Ok(());
                }
            }
        }
        Err(error) => {
            client_task.abort();
            return Err(anyhow::anyhow!("harness preparation task stopped: {error}"));
        }
    };

    let RunningHarness {
        mut acp_task,
        mut event_task,
        acp_shutdown,
        commands: acp_commands_tx,
        subagent_socket_guard,
        harness_gc,
        shell_cleanup,
        _managed_harness,
        _managed_node,
        project_tools,
        ..
    } = running_harness;

    let acp_result = async {
        let acp_join = tokio::select! {
            fatal = fatal_rx.recv() => {
                let error = fatal
                    .unwrap_or_else(|| anyhow::anyhow!("relay failure report was lost"));
                stop_failed_coordinator(&mut event_task).await;
                drop(acp_commands_tx);
                return stop_peer_and_return(
                    &mut acp_task,
                    &acp_shutdown,
                    error,
                    "relay durable state became unwritable",
                ).await;
            }
            accepted = &mut client_task => {
                let error = accepted
                    .context("worker proxy accept task stopped")?
                    .err()
                    .unwrap_or_else(|| anyhow::anyhow!("worker proxy accept task stopped"));
                stop_failed_coordinator(&mut event_task).await;
                drop(acp_commands_tx);
                return stop_peer_and_return(
                    &mut acp_task,
                    &acp_shutdown,
                    error,
                    "accept worker proxy",
                ).await;
            }
            result = &mut event_task => {
                match result {
                    Ok(Ok(())) => {
                        acp_shutdown.cancel();
                        Some(acp_task.await)
                    }
                    Ok(Err(error)) => {
                        drop(acp_commands_tx);
                        return stop_peer_and_return(
                            &mut acp_task,
                            &acp_shutdown,
                            error,
                            "relay coordinator failed",
                        ).await;
                    }
                    Err(error) => {
                        drop(acp_commands_tx);
                        return stop_peer_and_return(
                            &mut acp_task,
                            &acp_shutdown,
                            anyhow::anyhow!(error),
                            "relay coordinator task stopped",
                        ).await;
                    }
                }
            }
            result = &mut acp_task => {
                event_task.await.context("relay event task stopped")??;
                Some(result)
            }
            request = wait_for_suspend(&mut suspend_signal) => {
                suspend_request = Some(request?);
                acp_shutdown.cancel();
                stop_failed_coordinator(&mut event_task).await;
                let _ = acp_task.await;
                None
            }
            _ = &mut ctrl_c => {
                acp_shutdown.cancel();
                stop_failed_coordinator(&mut event_task).await;
                let _ = acp_task.await;
                None
            }
            _ = terminate.recv() => {
                acp_shutdown.cancel();
                stop_failed_coordinator(&mut event_task).await;
                let _ = acp_task.await;
                None
            }
        };
        let Some(acp_join) = acp_join else {
            return Ok(());
        };
        match acp_join {
            Ok(result) => result,
            Err(error) => {
                let error = anyhow::anyhow!("ACP runtime task stopped: {error}");
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .record_observation(RelayObservation::Warning {
                        message: format!("{error:#}"),
                    })?;
                Err(error)
            }
        }
    }
    .await;

    let closed = relay
        .lock()
        .expect("relay state lock poisoned")
        .operational_state()
        .execution
        == mj_core::relay::RelayExecutionState::Closed;
    // The install belongs to the harness: a closed relay stays attachable for
    // checkpointing, which must not see an installer still writing files.
    if let Some(mut install) = project_tools {
        install.stop().await;
    }
    shell_cleanup.close();
    shell_cleanup.wait().await;
    drop(subagent_socket_guard);
    if let Some(task) = harness_gc {
        task.abort();
    }
    if let Some(request) = suspend_request {
        client_task.abort();
        let _ = client_task.await;
        return finish_suspend(root, suspend_session_id, request).await;
    }
    if !closed {
        // Includes a harness that did not open its session: the coordinator's
        // typed startup failure ends the worker and reaches the exit record.
        client_task.abort();
        return acp_result;
    }
    if let Err(error) = &acp_result {
        tracing::warn!(%error, "ACP stopped with an error after the relay closed");
    }
    // A closed relay remains attachable so the controller can finish its
    // checkpoint. Harness-only operations are rejected.
    tokio::select! {
        fatal = fatal_rx.recv() => {
            client_task.abort();
            Err(fatal.unwrap_or_else(|| anyhow::anyhow!("relay failure report was lost"))
                .context("relay durable state became unwritable"))
        }
        accepted = &mut client_task => {
            let error = accepted
                .context("worker proxy accept task stopped")?
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("worker proxy accept task stopped"));
            Err(error)
        }
        _ = &mut ctrl_c => {
            client_task.abort();
            Ok(())
        }
        _ = terminate.recv() => {
            client_task.abort();
            Ok(())
        }
    }
}

async fn accept_worker_clients(
    listener: UnixListener,
    _socket_guard: SocketGuard,
    relay: Arc<RelayCell>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: SharedCredentials,
    runtime: ConnectionRuntime,
    fatal: mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    let mut clients = tokio::task::JoinSet::new();
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => accepted.context("accept worker proxy")?,
            completed = clients.join_next(), if !clients.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(%error, "worker proxy client task stopped");
                }
                continue;
            }
        };
        let client_relay = relay.clone();
        let client_dispatch_wake = dispatch_wake.clone();
        let client_credentials = credentials.clone();
        let client_runtime = runtime.clone();
        let client_fatal = fatal.clone();
        clients.spawn(async move {
            if let Err(error) = serve_client_with_shared_credentials(
                stream,
                client_relay,
                client_dispatch_wake,
                client_credentials,
                client_runtime,
                client_fatal,
            )
            .await
            {
                tracing::warn!(%error, "relay proxy client disconnected");
            }
        });
    }
}

async fn bounded_preparation_step<T>(
    budget: &PreparationStepBudget,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(budget.remaining(), operation).await {
        Ok(result) => result,
        Err(_) => Err(budget.deadline_error()),
    }
}

async fn bounded_blocking_preparation_step<T>(
    budget: &PreparationStepBudget,
    cancel: &CancellationToken,
    operation: impl FnOnce(CancellationToken) -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    // Synchronous filesystem/socket calls cannot be forcibly interrupted.
    // On timeout or cancellation this stops awaiting the thread; the
    // preparation's thread tracker (see `supervise_preparation`) still counts
    // it, so a cancelled preparation waits for it and a failed one ends the
    // worker, which ends the thread.
    let step_cancel = cancel.child_token();
    let operation_cancel = step_cancel.clone();
    let task = mj_core::runtime::spawn_blocking(move || operation(operation_cancel));
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            step_cancel.cancel();
            bail!("worker preparation step {} cancelled", budget.step);
        }
        result = tokio::time::timeout_at(budget.deadline, task) => {
            match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => Err(anyhow::Error::new(error)
                    .context(format!("worker preparation step {} stopped", budget.step))),
                Err(_) => {
                    step_cancel.cancel();
                    Err(budget.deadline_error())
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct PreparationGate(Mutex<Option<PreparationActivity>>);

struct PreparationActivity {
    attempt: String,
    step: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl PreparationGate {
    fn set_step(&self, step: &str, started_at: chrono::DateTime<chrono::Utc>) {
        let mut activity = self.0.lock().expect("preparation gate poisoned");
        let activity = activity.as_mut().expect("preparation writer owns the gate");
        activity.step = step.to_owned();
        activity.started_at = started_at;
    }

    fn release(&self) {
        self.0.lock().expect("preparation gate poisoned").take();
    }
}

tokio::task_local! {
    static ACTIVE_PREPARATION: Arc<PreparationGate>;
}

struct PreparationWriter {
    gate: Arc<PreparationGate>,
    threads: tokio_util::task::TaskTracker,
}

impl PreparationWriter {
    fn try_acquire(
        gate: &Arc<PreparationGate>,
        attempt: String,
    ) -> Result<Self, Box<RelayProtocolError>> {
        let mut activity = gate.0.lock().expect("preparation gate poisoned");
        if let Some(activity) = activity.as_ref() {
            return Err(Box::new(mj_core::relay::relay_protocol_error(
                RelayErrorCode::InvalidState,
                format!(
                    "{} is already in progress at {} (started {}). Retry after it finishes, or restart the worker or session if it is stalled.",
                    activity.attempt,
                    activity.step,
                    activity.started_at.to_rfc3339(),
                ),
                false,
                None,
            )));
        }
        *activity = Some(PreparationActivity {
            attempt,
            step: "initialize launch".into(),
            started_at: chrono::Utc::now(),
        });
        Ok(Self {
            gate: gate.clone(),
            threads: tokio_util::task::TaskTracker::new(),
        })
    }

    async fn track<T>(
        &self,
        cancel: &CancellationToken,
        preparation: impl std::future::Future<Output = T>,
    ) -> T {
        ACTIVE_PREPARATION
            .scope(
                self.gate.clone(),
                mj_core::runtime::track_blocking_work(
                    self.threads.clone(),
                    mj_core::subprocess::with_preparation_cancellation(
                        cancel.child_token(),
                        Box::pin(preparation),
                    ),
                ),
            )
            .await
    }
}

impl Drop for PreparationWriter {
    fn drop(&mut self) {
        self.threads.close();
        if self.threads.is_empty() {
            self.gate.release();
        } else {
            let gate = self.gate.clone();
            let threads = self.threads.clone();
            // Keep the gate occupied until detached blocking writers exit,
            // including when their request has stopped waiting for them.
            mj_core::runtime::spawn(async move {
                threads.wait().await;
                gate.release();
            });
        }
    }
}

async fn supervise_harness_preparation(
    setup: HarnessPreparationSetup,
    dispatch_wake_rx: mpsc::Receiver<()>,
    status: watch::Sender<PreparationSnapshot>,
    cancel: CancellationToken,
) -> PreparationOutcome {
    let root = setup.root.clone();
    let mut dispatch_wakes = Some(dispatch_wake_rx);
    let gate = setup.relay.preparation_writer.clone();
    let result = supervise_preparation(
        &root,
        &gate,
        &status,
        &cancel,
        prepare_and_start_harness(setup, &mut dispatch_wakes, &status, &cancel),
    )
    .await;
    match result {
        Ok(running) => PreparationOutcome::Started(Box::new(running)),
        Err(PreparationStop::Cancelled) => PreparationOutcome::Cancelled(dispatch_wakes),
        Err(PreparationStop::Failed(error)) => PreparationOutcome::Failed(error),
    }
}

/// Run one harness preparation to an end it owns: prepared, cancelled, or
/// failed with the typed startup failure the worker exits with.
///
/// Commands the preparation runs through the shared subprocess helpers stop
/// when it is cancelled or ends, including those on blocking threads, and
/// every thread it hands to the blocking pool is counted until it returns. A
/// failed preparation lets those threads stop their commands, within the
/// failed step's own bound, before the worker exits; the exit ends any thread
/// still blocked. A cancelled one returns at once: either the worker is
/// stopping, or it keeps serving a sealed relay while the cancelled threads
/// finish under their step's bound, and the controller stops the worker
/// before it removes any of its files.
async fn supervise_preparation<T>(
    root: &std::path::Path,
    gate: &Arc<PreparationGate>,
    status: &watch::Sender<PreparationSnapshot>,
    cancel: &CancellationToken,
    preparation: impl std::future::Future<Output = Result<T>>,
) -> std::result::Result<T, PreparationStop> {
    let writer = PreparationWriter::try_acquire(gate, "worker startup".into())
        .expect("worker startup is the first preparation on a new relay");
    let preparation = writer.track(cancel, preparation);
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = catch_preparation_panic(preparation) => Some(result.and_then(|result| result)),
    };
    // The preparation future is gone: its commands were stopped with it.
    writer.threads.close();
    let (step, step_deadline) = {
        let snapshot = status.borrow();
        let step = match &snapshot.state {
            Some(mj_core::relay::HarnessPreparation::Preparing { step, .. }) => step.clone(),
            _ => "bridge-start".to_owned(),
        };
        // Threads are bounded by the step that started them.
        let deadline = snapshot
            .step_deadline
            .unwrap_or_else(|| tokio::time::Instant::now() + LOCAL_PREPARATION_STEP_TIMEOUT);
        (step, deadline)
    };
    let error = match result {
        Some(Ok(prepared)) => return Ok(prepared),
        Some(Err(error)) if !cancel.is_cancelled() => error,
        // A step that saw the cancellation first fails with it; that is
        // still a cancellation.
        _ => return Err(PreparationStop::Cancelled),
    };
    let message = format!("harness preparation failed at {step}: {error:#}");
    let record_root = root.to_owned();
    let record_step = step.clone();
    let record_message = format!("{error:#}");
    let record = mj_core::runtime::spawn_blocking(move || {
        super::record_startup_failure(&record_root, &record_step, &record_message);
    });
    match tokio::time::timeout(LOCAL_PREPARATION_STEP_TIMEOUT, record).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, %step, "failed to record worker preparation failure")
        }
        Err(_) => {
            tracing::warn!(%step, "recording worker preparation failure exceeded its deadline")
        }
    }
    // Let commands the failed step left running stop before the worker exits,
    // within that step's bound (already spent when the step timed out); the
    // exit ends any thread that does not.
    if tokio::time::timeout_at(step_deadline, writer.threads.wait())
        .await
        .is_err()
    {
        tracing::warn!(%step, "a preparation thread was still running when preparation failed");
    }
    Err(PreparationStop::Failed(anyhow::Error::new(
        mj_core::relay::WorkerHarnessStartupFailure {
            message,
            kind: None,
        },
    )))
}

fn catch_preparation_panic<F>(future: F) -> impl std::future::Future<Output = Result<F::Output>>
where
    F: std::future::Future,
{
    let mut future = Box::pin(future);
    std::future::poll_fn(move |context| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            future.as_mut().poll(context)
        })) {
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(payload) => std::task::Poll::Ready(Err(anyhow::anyhow!(
                "harness preparation panicked: {}",
                panic_payload_message(payload.as_ref())
            ))),
        }
    })
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Fast part of project tools. Whatever mise reports missing is installed by
/// the returned [`super::project_tools::Install`] in the background; the agent
/// has already been told, so the harness starts at once.
async fn prepare_project_tools(
    root: &std::path::Path,
    roots: &[PathBuf],
    config: &mut WorkerLaunchConfig,
    base_environment: &BTreeMap<String, String>,
    status: Option<&watch::Sender<PreparationSnapshot>>,
    relay: &Arc<RelayCell>,
) -> Result<(bool, Option<super::project_tools::Install>)> {
    let mut environment = base_environment.clone();
    environment.extend(config.environment.clone());
    let mut applied = false;
    let result = async {
        let recipes = super::project_tools::recipe_roots(roots).await?;
        if recipes.is_empty() {
            // Without a recipe the image's own environment (its Rust homes,
            // PATH and tools) is left exactly as it was.
            return Ok(None);
        }
        super::project_tools::apply_environment(&mut environment, roots, None)?;
        config.environment = environment.clone();
        applied = true;
        let Some(status) = status else {
            let _ = ACTIVE_PREPARATION
                .try_with(|gate| gate.set_step("project tools", chrono::Utc::now()));
            // The running worker already owns any install in progress. Resolve
            // the same mise runtime and PATH without starting another install.
            let mise =
                mj_harness_install::harness_install::ensure_mise_runtime(&environment).await?;
            super::project_tools::apply_environment(&mut environment, roots, Some(&mise))?;
            config.environment = environment;
            return Ok(None);
        };
        status.send_modify(|snapshot| {
            snapshot.state = Some(mj_core::relay::HarnessPreparation::Preparing {
                step: "Install project tools".into(),
                since_ms: chrono::Utc::now().timestamp_millis(),
            });
            // Planning has no bound of its own; its threads get a local step's.
            snapshot.step_deadline =
                Some(tokio::time::Instant::now() + LOCAL_PREPARATION_STEP_TIMEOUT);
        });
        let breadcrumb_root = root.to_owned();
        mj_core::runtime::spawn_blocking(move || {
            super::record_startup_step(&breadcrumb_root, "Install project tools");
        })
        .await
        .context("record project tools startup step")?;
        // Installing mise itself (when the image has none) and asking what is
        // missing are short; the installation proper runs after the harness
        // starts. The preparation owner cancels this future on close.
        let install = super::project_tools::plan(root, &recipes, &mut environment).await?;
        config.environment = environment.clone();
        if let Some(install) = &install {
            install.tell_agent_started(relay)?;
        }
        anyhow::Ok(install)
    }
    .await;
    let install = match result {
        Ok(install) => install,
        Err(error) => {
            let message = format!(
                "Project tools preparation failed: {error:#}. See {}. The session will continue.",
                root.join(super::project_tools::LOG_FILE).display()
            );
            let log_path = root.join(super::project_tools::LOG_FILE);
            match tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
                .await
            {
                Ok(mut log) => {
                    use tokio::io::AsyncWriteExt;
                    if let Err(error) = log.write_all(format!("{message}\n").as_bytes()).await {
                        tracing::warn!(%error, "write project tools failure log");
                    }
                }
                Err(error) => tracing::warn!(%error, "open project tools failure log"),
            }
            relay
                .lock()
                .expect("relay state lock poisoned")
                .record_observation(RelayObservation::Notice { message })?;
            None
        }
    };
    Ok((applied, install))
}

fn initialize_launch_environment(
    config: &mut WorkerLaunchConfig,
    startup_directory: &std::path::Path,
) {
    let mut environment = config.target_environment.clone();
    environment.append(&mut config.environment);
    for directive in [
        mj_core::worker_launch::SESSION_NITRO_MBX_ENV,
        mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH,
        mj_core::jev::FASTINO_KEY_ENVIRONMENT,
    ] {
        environment.remove(directive);
    }
    config.environment = environment;
    super::resolve_relative_harness_home(config, startup_directory);
    if config.no_github_credentials {
        mj_core::credentials::remove_github_authentication_environment(&mut config.environment);
        mj_core::credentials::remove_github_authentication_environment(
            &mut config.target_environment,
        );
    }
}

/// What the harness's own sandbox profile is built from: the directories the
/// ACP session is given, and the roots whose Git directories it needs.
fn harness_sandbox(
    config: &WorkerLaunchConfig,
    base_environment: &BTreeMap<String, String>,
) -> super::git_sandbox::HarnessSandbox {
    let mut environment = base_environment.clone();
    environment.extend(config.environment.clone());
    super::git_sandbox::HarnessSandbox {
        directories: super::acp_session_directories(config, &environment),
        git_roots: std::iter::once(config.cwd.clone())
            .chain(config.additional_directories.iter().cloned())
            .chain(config.writable_directories.iter().cloned())
            .collect(),
    }
}

struct PreparedHarnessEnvironment {
    launch: super::harness_launch::PreparedHarnessLaunch,
    git_setup: tempfile::TempDir,
    profile_registration: bool,
    managed_subagent: bool,
    subagent_role: Option<mj_core::subagent::SubagentMcpRole>,
    boundary_mailbox_hook_available: bool,
    mailbox_delivery_mode: Option<mj_core::mailbox::MailboxDeliveryMode>,
    project_tools_install: Option<super::project_tools::Install>,
}

/// Build the process environment and its worker-owned files for every primary
/// harness launch, including a replacement of the bridge in a running worker.
async fn apply_profile_configuration(
    root: &Path,
    config: &mut WorkerLaunchConfig,
    capabilities: ProfileCapabilities,
    base_environment: &BTreeMap<String, String>,
    status: Option<&watch::Sender<PreparationSnapshot>>,
    cancel: &CancellationToken,
) -> Result<(bool, bool, Option<mj_core::mailbox::MailboxDeliveryMode>)> {
    let credentials = super::credential_endpoint(config);
    let ProfileCapabilities {
        managed_subagent,
        review_tools,
        subagent_role,
    } = capabilities;
    // Every launch records its `gh` approval where the selected harness reads
    // it, and OpenCode's denials apply in every execution policy.
    {
        let budget = preparation_step(
            root,
            status,
            "command-permissions",
            LOCAL_PREPARATION_STEP_TIMEOUT,
            cancel,
        )
        .await?;
        let home = credentials
            .as_ref()
            .map_err(|message| anyhow::anyhow!("{message}"))?
            .home
            .clone();
        let root = root.to_owned();
        let session_id = config.session_id.clone();
        let harness = config.harness;
        let policy = config.execution_policy;
        // A child, a None choice and a Mjolnir choice all leave delegation to
        // Mjolnir (or to nobody), so OpenCode's own `subagent` tool goes.
        let denials = super::json_gh_rules::OpenCodeDenials {
            native_subagent: config.subagents.suppresses_native(),
        };
        let native_base_environment = base_environment.clone();
        let native_explicit_environment = config.environment.clone();
        let mut environment = native_base_environment.clone();
        environment.extend(native_explicit_environment.clone());
        config.environment =
            bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                if step_cancel.is_cancelled() {
                    bail!("preparation cancelled before configuring command permissions");
                }
                super::gh_rules::configure_native_rules(
                    &root,
                    &session_id,
                    &home,
                    harness,
                    policy,
                    denials,
                    &mut environment,
                )?;
                environment.retain(|name, value| {
                    native_explicit_environment.contains_key(name)
                        || native_base_environment.get(name) != Some(value)
                });
                Ok(environment)
            })
            .await?;
    }

    let mut boundary_mailbox_hook_available = false;
    let mut mailbox_delivery_mode = None;
    let profile_registration = match config.harness {
        HarnessKind::Codex => {
            let budget = preparation_step(
                root,
                status,
                "subagent-profile",
                LOCAL_PREPARATION_STEP_TIMEOUT,
                cancel,
            )
            .await?;
            let home = credentials
                .as_ref()
                .map_err(|message| anyhow::anyhow!("{message}"))?
                .home
                .clone();
            let root = root.to_owned();
            let session_id = config.session_id.clone();
            let mut environment = config.environment.clone();
            let role = subagent_role;
            let policy = config.execution_policy;
            let subtle_subagents = config.subtle_subagents;
            let sandbox = harness_sandbox(config, base_environment);
            let (registered, environment, mode) =
                bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                    if step_cancel.is_cancelled() {
                        bail!("preparation cancelled before configuring the Codex profile");
                    }
                    let registered = super::subagents::configure_codex_profile(
                        &root,
                        &session_id,
                        &home,
                        &mut environment,
                        role,
                        policy,
                        subtle_subagents,
                        review_tools,
                        managed_subagent,
                        &sandbox,
                    )?;
                    // The staged home holds the configuration Codex runs
                    // with, overlay included. An unreadable one fails startup
                    // here rather than guessing a delivery mode.
                    let provider = mj_core::codex_provider::codex_provider(&home, None)
                        .context("resolve the Codex provider for mailbox delivery")?;
                    let mode = mj_core::mailbox::MailboxDeliveryMode::for_codex_provider(
                        provider.as_ref(),
                    );
                    Ok((registered, environment, mode))
                })
                .await?;
            config.environment = environment;
            mailbox_delivery_mode = Some(mode);
            registered
        }
        HarnessKind::Claude => {
            let budget = preparation_step(
                root,
                status,
                "harness-profile",
                LOCAL_PREPARATION_STEP_TIMEOUT,
                cancel,
            )
            .await?;
            let home = credentials
                .as_ref()
                .map_err(|message| anyhow::anyhow!("{message}"))?
                .home
                .clone();
            let root = root.to_owned();
            let session_id = config.session_id.clone();
            let role = subagent_role;
            let capabilities = (config.oracle, config.orchestrators_enabled);
            let subtle_subagents = config.subtle_subagents;
            let policy = config.execution_policy;
            let sandbox = harness_sandbox(config, base_environment);
            if managed_subagent {
                // Marketplace refreshes on every child start fetch from Git or
                // HTTP sources; the child's staged plugins are copied from
                // the person's profile, which keeps refreshing them. The same
                // switch stops Claude Code's own updater, which the pinned
                // managed install does not use.
                config
                    .environment
                    .entry("DISABLE_AUTOUPDATER".into())
                    .or_insert_with(|| "1".into());
                // Claude Code otherwise reminds a child that has gone several
                // steps without text that "the user hasn't heard from you",
                // prompting status messages nobody reads; the parent sees
                // only the child's report.
                config
                    .environment
                    .entry("CLAUDE_CODE_SILENT_TURN_REMINDER".into())
                    .or_insert_with(|| "false".into());
            }
            // `config.environment` keeps only overrides; the login shell's
            // HOME, which the harness inherits, is in the base environment.
            let user_home = config
                .environment
                .get("HOME")
                .or_else(|| base_environment.get("HOME"))
                .map(PathBuf::from);
            let (registered, mailbox_hook_available) =
                bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                    if step_cancel.is_cancelled() {
                        bail!("preparation cancelled before configuring the Claude profile");
                    }
                    let registered = super::subagents::configure_claude_subagent_mcp(
                        &root,
                        &session_id,
                        &home,
                        role,
                        capabilities,
                        subtle_subagents,
                        review_tools,
                    )?;
                    let mailbox_hook_available =
                        super::subagents::configure_claude_mailbox_hook(&root, &session_id, &home)?;
                    super::subagents::exclude_default_claude_instructions(
                        &root,
                        &session_id,
                        &home,
                        user_home.as_deref(),
                    )?;
                    if managed_subagent {
                        super::subagents::configure_claude_subagent_settings(
                            &root,
                            &session_id,
                            &home,
                        )?;
                    }
                    super::git_sandbox::configure_claude_git_sandbox(
                        &root,
                        &session_id,
                        &home,
                        policy,
                        &sandbox,
                    )?;
                    Ok((registered, mailbox_hook_available))
                })
                .await
                .context("prepare Claude profile")?;
            boundary_mailbox_hook_available = mailbox_hook_available;
            registered
        }
        HarnessKind::Antigravity => {
            let budget = preparation_step(
                root,
                status,
                "harness-profile",
                LOCAL_PREPARATION_STEP_TIMEOUT,
                cancel,
            )
            .await?;
            let home = credentials
                .as_ref()
                .map_err(|message| anyhow::anyhow!("{message}"))?
                .home
                .clone();
            let root = root.to_owned();
            let session_id = config.session_id.clone();
            let mailbox_hook_available =
                bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                    if step_cancel.is_cancelled() {
                        bail!("preparation cancelled before configuring the Antigravity profile");
                    }
                    super::subagents::configure_antigravity_mailbox_hook(&root, &session_id, &home)
                })
                .await
                .context("prepare Antigravity profile")?;
            boundary_mailbox_hook_available = mailbox_hook_available;
            mailbox_delivery_mode = Some(if mailbox_hook_available {
                mj_core::mailbox::MailboxDeliveryMode::ToolBoundary
            } else {
                mj_core::mailbox::MailboxDeliveryMode::TurnBoundary
            });
            false // Antigravity receives its MCP servers through ACP.
        }
        HarnessKind::OpenCode => {
            let budget = preparation_step(
                root,
                status,
                "mailbox-plugin",
                LOCAL_PREPARATION_STEP_TIMEOUT,
                cancel,
            )
            .await?;
            let root = root.to_owned();
            let mut environment = config.environment.clone();
            // Under configured approvals the Mjolnir guardian answers
            // OpenCode's permission requests, and its hook makes OpenCode ask.
            let guardian_roots = (config.execution_policy
                == mj_core::config::ExecutionPolicy::ConfiguredApprovals)
                .then(|| {
                    std::iter::once(config.cwd.clone())
                        .chain(config.additional_directories.iter().cloned())
                        .chain(config.writable_directories.iter().cloned())
                        .collect::<Vec<_>>()
                });
            config.environment =
                bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                    if step_cancel.is_cancelled() {
                        bail!(
                            "preparation cancelled before configuring the OpenCode mailbox plugin"
                        );
                    }
                    super::subagents::configure_opencode_mailbox_hook(&root, &mut environment)?;
                    if let Some(roots) = &guardian_roots {
                        super::json_gh_rules::configure_opencode_guardian(
                            &root,
                            roots,
                            &mut environment,
                        )?;
                    }
                    Ok(environment)
                })
                .await?;
            false // OpenCode receives its MCP servers through ACP.
        }
        HarnessKind::Kimi if subagent_role.is_some() => {
            let budget = preparation_step(
                root,
                status,
                "harness-profile",
                LOCAL_PREPARATION_STEP_TIMEOUT,
                cancel,
            )
            .await?;
            let home = credentials
                .as_ref()
                .map_err(|message| anyhow::anyhow!("{message}"))?
                .home
                .clone();
            let root = root.to_owned();
            let session_id = config.session_id.clone();
            let role = subagent_role.context("Kimi session has no MCP role")?;
            bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                if step_cancel.is_cancelled() {
                    bail!("preparation cancelled before configuring the Kimi profile");
                }
                super::subagents::configure_kimi_subagent_mcp(
                    &root,
                    &session_id,
                    &home,
                    role,
                    review_tools,
                )
            })
            .await?;
            true
        }
        _ => false,
    };
    let mut cache_environment = base_environment.clone();
    cache_environment.extend(config.environment.clone());
    super::harness_launch::link_build_cache_configuration(&cache_environment).await?;
    Ok((
        profile_registration,
        boundary_mailbox_hook_available,
        mailbox_delivery_mode,
    ))
}

fn apply_profile_environment_delta(
    config: &WorkerLaunchConfig,
    before: &BTreeMap<String, String>,
    environment: &mut BTreeMap<String, String>,
    spec: &mut super::AcpSupervisorSpec,
) {
    for name in before
        .keys()
        .filter(|name| !config.environment.contains_key(*name))
    {
        environment.remove(name);
        spec.environment.remove(name);
    }
    for (name, value) in &config.environment {
        if before.get(name) != Some(value) {
            environment.insert(name.clone(), value.clone());
            spec.environment.insert(name.clone(), value.clone());
        }
    }
    for name in config
        .excluded_environment
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(mj_core::jev::FASTINO_KEY_ENVIRONMENT))
    {
        environment.remove(name);
        spec.environment.remove(name);
    }
}

async fn prepare_harness_environment(
    root: &Path,
    config: &mut WorkerLaunchConfig,
    relay: &Arc<RelayCell>,
    status: Option<&watch::Sender<PreparationSnapshot>>,
    cancel: &CancellationToken,
    record_launch_baseline: bool,
) -> Result<PreparedHarnessEnvironment> {
    let mut prepared =
        prepare_harness_private(root, config, relay, status, cancel, record_launch_baseline)
            .await?;
    let before = config.environment.clone();
    let (registered, boundary, mailbox) = apply_profile_configuration(
        root,
        config,
        ProfileCapabilities {
            managed_subagent: prepared.managed_subagent,
            subagent_role: prepared.subagent_role,
            review_tools: config.review_tools(),
        },
        &prepared.launch.environment,
        status,
        cancel,
    )
    .await?;
    apply_profile_environment_delta(
        config,
        &before,
        &mut prepared.launch.environment,
        &mut prepared.launch.spec,
    );
    prepared.profile_registration = registered;
    prepared.boundary_mailbox_hook_available = boundary;
    prepared.mailbox_delivery_mode = mailbox;
    Ok(prepared)
}

async fn prepare_harness_private(
    root: &std::path::Path,
    config: &mut WorkerLaunchConfig,
    relay: &Arc<RelayCell>,
    status: Option<&watch::Sender<PreparationSnapshot>>,
    cancel: &CancellationToken,
    record_launch_baseline: bool,
) -> Result<PreparedHarnessEnvironment> {
    let project_tools_roots = super::project_tools::take_roots(&mut config.environment)?;
    let managed_subagent = config.is_managed_subagent();
    let credentials = super::credential_endpoint(config);
    let subagent_role = config.subagent_mcp_role();
    let login_budget = preparation_step(
        root,
        status,
        "login-resolve",
        LOCAL_PREPARATION_STEP_TIMEOUT,
        cancel,
    )
    .await?;
    // Keep non-Podman Git includes pinned to the worker's original home.
    // Podman supplies its provisioned config path separately because root
    // worker processes can have HOME=/root.
    let worker_home = std::env::var_os("HOME").map(PathBuf::from);
    let base_environment = bounded_preparation_step(&login_budget, async {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mj_harness_install::login_environment::resolve(),
        )
        .await
        .context("login environment resolution exceeded its 10-second bound")?
    })
    .await?;
    if config.harness == HarnessKind::OpenCode {
        // Retained launch files can predate the explicit OpenCode config pin.
        let home = super::credential_endpoint(config)
            .map_err(anyhow::Error::msg)?
            .home;
        config
            .harness
            .configure_staged_home_environment(&home, &mut config.environment);
    }
    let record_raw_launch = config
        .environment
        .get(mj_core::worker_launch::SESSION_RAW_LAUNCH_RECORD_ENV)
        .is_some_and(|value| value == "1");
    let mut session_environment = base_environment.clone();
    session_environment.extend(config.environment.clone());
    session_environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    session_environment.remove(mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV);
    session_environment.remove(mj_core::worker_launch::SESSION_RAW_LAUNCH_RECORD_ENV);
    session_environment.remove(mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV);
    session_environment.remove(mj_core::worker_launch::SESSION_PROJECT_TOOLS_ROOTS);
    super::restore_mbx_launcher(root, worker_home.as_deref(), &mut session_environment)?;
    let github_root = root.to_owned();
    let no_github_credentials = config.no_github_credentials;
    let github_home = worker_home.clone();
    let initial_launch = status.is_some();
    let git_config_include = config
        .target_environment
        .get(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH)
        .map(PathBuf::from);
    let mut explicit_environment = config.environment.clone();
    explicit_environment.remove(mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV);
    explicit_environment.remove(mj_core::worker_launch::SESSION_RAW_LAUNCH_RECORD_ENV);
    explicit_environment.remove(mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV);
    let base_environment_for_filter = base_environment.clone();
    // Credential sync installs the session's slopcop login in its harness home.
    let slopcop_login = credentials
        .as_ref()
        .map(|endpoint| endpoint.home.join(mj_core::slopcop::LOGIN_FILE))
        .map_err(Clone::clone);
    let (session_environment, git_setup) =
        bounded_blocking_preparation_step(&login_budget, cancel, move |step_cancel| {
            if step_cancel.is_cancelled() {
                anyhow::bail!("preparation cancelled before resolving Git credentials");
            }
            let setup = new_git_setup(&github_root)?;
            let token_path = github_root.join("github-token");
            configure_git_setup(
                setup.path(),
                &token_path,
                &mut session_environment,
                github_home.as_deref(),
                git_config_include.as_deref(),
                no_github_credentials,
            )?;
            if initial_launch && no_github_credentials {
                // There is no running harness to protect on worker startup.
                // A restart can remove this token only after its reload succeeds.
                mj_core::credentials::remove_github_token(&token_path)?;
            }
            match &slopcop_login {
                Ok(login) => configure_slopcop_cli(setup.path(), login)?,
                Err(error) => {
                    tracing::warn!("slopcop wrapper not installed: {error}");
                }
            }
            session_environment.retain(|name, value| {
                explicit_environment.contains_key(name)
                    || base_environment_for_filter.get(name) != Some(value)
            });
            Ok((session_environment, setup))
        })
        .await?;
    // Persist only explicit and Mjolnir-generated overrides, never shell exports.
    config.environment = session_environment.clone();

    if record_raw_launch && record_launch_baseline {
        let budget = preparation_step(
            root,
            status,
            "raw-launch-record",
            LOCAL_PREPARATION_STEP_TIMEOUT,
            cancel,
        )
        .await?;
        let cwd = config.cwd.clone();
        let session_id = config.session_id.clone();
        bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
            if step_cancel.is_cancelled() {
                bail!("preparation cancelled before recording raw launch baseline");
            }
            if let Err(error) = mj_checkpoint::snapshot::record_launch_if_absent(
                &mj_checkpoint::archive::SystemGit,
                &cwd,
                &session_id,
            ) {
                // Capture falls back to keeping all local refs and stashes when
                // the launch record is absent, so a read-only Git directory
                // must not block starting the session.
                tracing::warn!(
                    session_id = %session_id,
                    cwd = %cwd.display(),
                    error = format!("{error:#}"),
                    "could not record raw launch baseline; continuing without baseline"
                );
            }
            Ok(())
        })
        .await?;
    }

    if status.is_some() && config.prepares_review() {
        let budget = preparation_step(
            root,
            status,
            "review-gate",
            LOCAL_PREPARATION_STEP_TIMEOUT,
            cancel,
        )
        .await?;
        let launch = config.clone();
        let journal = relay.clone();
        bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
            if step_cancel.is_cancelled() {
                bail!("review gate preparation cancelled");
            }
            let notices = super::harness_launch::prepare_review_gate(&launch)?;
            let mut relay = journal.lock().expect("relay state lock poisoned");
            for message in notices {
                relay.record_observation(RelayObservation::Warning { message })?;
            }
            Ok(())
        })
        .await?;
    }

    let (project_tools_applied, project_tools_install) = match &project_tools_roots {
        Some(roots) => {
            prepare_project_tools(root, roots, config, base_environment, status, relay).await?
        }
        None => (false, None),
    };

    let keep_budget = preparation_step(
        root,
        status,
        "session-keep",
        LOCAL_PREPARATION_STEP_TIMEOUT,
        cancel,
    )
    .await?;
    let keep_cwd = config.cwd.clone();
    let keep_session_id = config.session_id.clone();
    let mut keep_environment = config.environment.clone();
    config.environment =
        bounded_blocking_preparation_step(&keep_budget, cancel, move |step_cancel| {
            if step_cancel.is_cancelled() {
                bail!("preparation cancelled before preparing the session keep directory");
            }
            if let Err(error) = mj_core::session_keep::install_session_keep_dir(
                &keep_cwd,
                &keep_session_id,
                &mut keep_environment,
            ) {
                // A read-only checkout, or a symlinked `.mj` path, must not
                // block the session. The harness then has no `MJ_KEEP_DIR`.
                tracing::warn!(
                    session_id = %keep_session_id,
                    cwd = %keep_cwd.display(),
                    error = format!("{error:#}"),
                    "could not prepare the session keep directory; continuing without MJ_KEEP_DIR"
                );
            }
            Ok(keep_environment)
        })
        .await?;

    let harness_budget = preparation_step(
        root,
        status,
        "harness-resolve",
        std::time::Duration::from_secs(20 * 60),
        cancel,
    )
    .await?;
    let (mut prepared_harness, git_setup) = bounded_preparation_step(&harness_budget, async {
        let mut prepared = super::harness_launch::prepare_harness_private_launch(
            config.harness,
            config.harness_runtime,
            config.execution_policy,
            super::harness_launch::primary_supervisor_spec(config),
        )
        .await?;
        let git_setup =
            super::harness_launch::freeze_node_entrypoints(git_setup, &mut prepared).await?;
        Ok((prepared, git_setup))
    })
    .await?;
    if let Some(roots) = project_tools_roots
        .as_ref()
        .filter(|_| project_tools_applied)
    {
        if let Err(error) =
            super::project_tools::apply_environment(&mut prepared_harness.environment, roots, None)
        {
            relay.lock().expect("relay state lock poisoned").record_observation(
                RelayObservation::Notice { message: format!("Project tools environment could not be applied: {error:#}. The session will continue.") }
            )?;
        }
        // Managed bridge preparation can prepend its own Node bin directory.
        // Restore the same session tool environment before launching the bridge.
        prepared_harness
            .spec
            .environment
            .extend(prepared_harness.environment.clone());
    }
    Ok(PreparedHarnessEnvironment {
        launch: prepared_harness,
        git_setup,
        profile_registration: false,
        managed_subagent,
        subagent_role,
        boundary_mailbox_hook_available: false,
        mailbox_delivery_mode: None,
        project_tools_install,
    })
}

async fn prepare_and_start_harness(
    setup: HarnessPreparationSetup,
    dispatch_wake_rx: &mut Option<mpsc::Receiver<()>>,
    status: &watch::Sender<PreparationSnapshot>,
    cancel: &CancellationToken,
) -> Result<RunningHarness> {
    let HarnessPreparationSetup {
        shared_subagents,
        antigravity_refresh,
        root: root_path,
        mut config,
        relay,
        restarting,
        task_monitors,
        resume_session,
        native_session_history,
        capture,
    } = setup;
    let root = root_path.as_path();
    let PreparedHarnessEnvironment {
        launch: prepared_harness,
        git_setup,
        profile_registration,
        managed_subagent,
        subagent_role,
        boundary_mailbox_hook_available,
        mailbox_delivery_mode,
        project_tools_install,
    } = prepare_harness_environment(root, &mut config, &relay, Some(status), cancel, !restarting)
        .await?;
    {
        let mut relay = relay.lock().expect("relay state lock poisoned");
        relay.set_steering_delivery_mode(config.harness, boundary_mailbox_hook_available);
        // Resolved before the harness starts, so no hook can drain earlier.
        if let Some(mode) = mailbox_delivery_mode {
            relay.set_mailbox_delivery_mode(mode);
        }
    }
    let managed_cache_root = prepared_harness
        .managed
        .as_ref()
        .map(|managed| managed.cache_root.clone());
    let managed_harness = prepared_harness.managed;
    let managed_node = prepared_harness.managed_node;

    let acp_budget = preparation_step(
        root,
        Some(status),
        "acp-setup",
        LOCAL_PREPARATION_STEP_TIMEOUT,
        cancel,
    )
    .await?;
    let acp_setup = AcpPreparationSetup {
        shared_subagents,
        root: root.to_owned(),
        config,
        session_environment: prepared_harness.environment,
        supervisor_spec: prepared_harness.spec,
        git_setup,
        relay: relay.clone(),
        resume_session,
        native_session_history,
        profile_registration,
        managed_subagent,
        subagent_role,
        runtime: tokio::runtime::Handle::current(),
        managed_cache_root,
    };
    let prepared_acp = bounded_blocking_preparation_step(&acp_budget, cancel, move |step_cancel| {
        if step_cancel.is_cancelled() {
            bail!("preparation cancelled before ACP setup");
        }
        build_acp_setup(acp_setup)
    })
    .await?;

    preparation_step(
        root,
        Some(status),
        "bridge-start",
        LOCAL_PREPARATION_STEP_TIMEOUT,
        cancel,
    )
    .await?;
    let install_relay = relay.clone();
    // This stage only schedules prepared async tasks on the current runtime;
    // it performs no filesystem or process I/O and cannot block a runtime
    // thread, so it needs no bound of its own.
    let started = start_bridge(
        prepared_acp,
        antigravity_refresh,
        relay,
        dispatch_wake_rx,
        task_monitors,
        capture,
        tokio::runtime::Handle::current(),
        cancel,
        status,
    )?;
    let mut running = started.into_running();
    running._managed_harness = managed_harness;
    running._managed_node = managed_node;
    running.project_tools = project_tools_install.map(|install| {
        super::project_tools::BackgroundInstall::spawn(install, root.to_owned(), install_relay)
    });

    Ok(running)
}

/// Stop a preparation the worker no longer wants, including a harness it
/// started just before the stop.
async fn stop_preparation(
    cancel: &CancellationToken,
    task: &mut tokio::task::JoinHandle<PreparationOutcome>,
) {
    cancel.cancel();
    if let Ok(PreparationOutcome::Started(running)) = task.await {
        shutdown_running_harness(*running).await;
    }
}

async fn shutdown_running_harness(mut running: RunningHarness) {
    running.acp_shutdown.cancel();
    stop_failed_coordinator(&mut running.event_task).await;
    if let Err(error) = running.acp_task.await {
        tracing::warn!(%error, "ACP runtime stopped while cancelling harness preparation");
    }
    running.shell_cleanup.close();
    running.shell_cleanup.wait().await;
    drop(running.subagent_socket_guard);
    if let Some(task) = running.harness_gc {
        task.abort();
    }
}

/// Install or validate the managed harness named by a proposed launch config
/// without starting, stopping, or otherwise touching the session worker.
///
/// The preparation owns the runtime lease for the whole call, so preparation
/// and the supervisor that later uses the same runtime never overlap.
pub async fn prepare_managed_harness(mut config: WorkerLaunchConfig) -> Result<()> {
    let mut environment = config.target_environment.clone();
    environment.remove(mj_core::worker_launch::SESSION_NITRO_MBX_ENV);
    environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    environment.extend(config.environment);
    environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    config.environment = environment;
    if config.requires_harness_preparation() {
        super::prepare_harness_launch(
            config.harness,
            config.harness_runtime,
            config.execution_policy,
            super::harness_launch::primary_supervisor_spec(&config),
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn serve_terminal_relay(
    listener: UnixListener,
    relay: Arc<RelayCell>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    runtime: ConnectionRuntime,
    fatal: mpsc::Sender<anyhow::Error>,
    mut fatal_reports: mpsc::Receiver<anyhow::Error>,
) -> Result<()> {
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => {
                accepted.context("accept closed relay proxy")?.0
            }
            report = fatal_reports.recv() => {
                return Err(report
                    .unwrap_or_else(|| anyhow::anyhow!("relay failure report was lost"))
                    .context("relay durable state became unwritable"));
            }
        };
        let client_relay = relay.clone();
        let client_dispatch_wake = dispatch_wake.clone();
        let client_credentials = credentials.clone();
        let client_fatal = fatal.clone();
        let client_runtime = runtime.clone();
        mj_core::runtime::spawn(async move {
            // A sealed session has no ACP runtime left, so compaction
            // cannot be served here.
            if let Err(error) = serve_client_with_memory(
                stream,
                client_relay,
                client_dispatch_wake,
                client_credentials,
                client_runtime,
                client_fatal,
            )
            .await
            {
                tracing::warn!(%error, "closed relay proxy client disconnected");
            }
        });
    }
}

/// The live services one relay connection can reach beyond the durable relay:
/// everything answered on the connection instead of being journaled.
#[derive(Clone, Default)]
pub(super) struct ConnectionRuntime {
    pub(super) shared_subagents: SharedSubagents,
    pub(super) preparation_tx: Option<watch::Sender<PreparationSnapshot>>,
    pub(super) antigravity_refresh: Option<mpsc::Sender<oneshot::Sender<Option<u64>>>>,
    pub(super) project_memory: ProjectMemoryEndpoint,
    /// The ACP coordinator's command channel, or `None` once the session is
    /// sealed and no ACP runtime is left to serve scratch prompts.
    pub(super) commands: Option<mpsc::Sender<CommandRequest>>,
    /// Capture state prepared with the primary harness. A sealed session has none.
    pub(super) reviewer: Option<Arc<ReviewCapture>>,
    pub(super) subagents: Option<super::subagents::SubagentEndpoint>,
    pub(super) cpu: Option<tokio::sync::watch::Receiver<crate::cpu_usage::CpuRead>>,
    pub(super) preparation: Option<watch::Receiver<PreparationSnapshot>>,
    pub(super) preparation_cancel: Option<CancellationToken>,
    pub(super) has_subagent_tools: bool,
}

#[cfg(test)]
pub(super) async fn serve_client(
    stream: UnixStream,
    relay: Arc<RelayCell>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    commands: Option<mpsc::Sender<CommandRequest>>,
    fatal: mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    serve_client_with_memory(
        stream,
        relay,
        dispatch_wake,
        credentials,
        ConnectionRuntime {
            commands,
            ..ConnectionRuntime::default()
        },
        fatal,
    )
    .await
}

pub(super) async fn serve_client_with_memory(
    stream: UnixStream,
    relay: Arc<RelayCell>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    runtime: ConnectionRuntime,
    fatal: mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    serve_client_with_shared_credentials(
        stream,
        relay,
        dispatch_wake,
        SharedCredentials::new(credentials),
        runtime,
        fatal,
    )
    .await
}

pub(super) async fn serve_client_with_shared_credentials(
    stream: UnixStream,
    relay: Arc<RelayCell>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: SharedCredentials,
    runtime: ConnectionRuntime,
    fatal: mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    let ConnectionRuntime {
        shared_subagents,
        preparation_tx,
        antigravity_refresh,
        project_memory,
        commands,
        reviewer,
        subagents,
        cpu,
        preparation,
        preparation_cancel,
        has_subagent_tools,
    } = runtime;
    let relay_root = relay
        .lock()
        .expect("relay state lock poisoned")
        .root()
        .to_path_buf();
    let session_id = relay
        .lock()
        .expect("relay state lock poisoned")
        .operational_state()
        .session_id;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut checkpoint_barriers = BTreeSet::new();
    let mut subagent_feed = super::subagents::SubagentFeed::default();
    let mut guardian_generation = 0;
    let mut project_memory_transfer = ConnectionProjectMemoryTransfer::default();
    let mut live_baseline_download = None;
    let serving_result = async {
        while let Some(line) =
            read_bounded_line(&mut reader, mj_core::relay::MAX_FRAME_BYTES).await?
        {
            // One decoder owns this boundary: an unknown method is a
            // protocol error the controller can act on, and an unreadable
            // frame is answered rather than dropping the connection.
            let envelope = match decode_relay_request(line.as_bytes()) {
                DecodedRelayRequest::Known(envelope) => *envelope,
                DecodedRelayRequest::Unknown {
                    request_id,
                    protocol_version,
                    method,
                } => {
                    let response =
                        unsupported_relay_method_response(request_id, protocol_version, method);
                    write_logged_response(&mut writer, &response, &session_id, "unknown").await?;
                    continue;
                }
                DecodedRelayRequest::Invalid {
                    request_id,
                    protocol_version,
                    message,
                } => {
                    let response =
                        invalid_relay_request_response(request_id, protocol_version, message);
                    write_logged_response(&mut writer, &response, &session_id, "invalid").await?;
                    continue;
                }
            };
            if let Some(body) = mj_core::relay::protocol::relay_protocol_rejection(&envelope) {
                let response = RelayResponseEnvelope {
                    request_id: envelope.request_id,
                    protocol_version: envelope.protocol_version,
                    body,
                };
                write_logged_response(&mut writer, &response, &session_id, "incompatible").await?;
                continue;
            }
            let prep_snapshot = preparation
                .as_ref()
                .map(|receiver| receiver.borrow().clone())
                .unwrap_or_default();
            let prepared_services = prep_snapshot.services.clone();
            let request_commands = prepared_services
                .as_ref()
                .map(|services| services.commands.clone())
                .or_else(|| commands.clone());
            let request_reviewer = prepared_services
                .as_ref()
                .map(|services| services.reviewer.clone())
                .or_else(|| reviewer.clone());
            let request_subagents = prepared_services
                .as_ref()
                .and_then(|services| services.subagents.clone())
                .or_else(|| shared_subagents.endpoint())
                .or_else(|| subagents.clone());
            if let RelayRequest::WaitForChange {
                after_ordinal,
                state_digest,
            } = &envelope.request
            {
                let body = match wait_for_change(
                    &relay,
                    preparation.as_ref(),
                    request_subagents.as_ref(),
                    &subagent_feed,
                    &mut guardian_generation,
                    *after_ordinal,
                    state_digest.as_deref(),
                )
                .await
                {
                    Ok(body) => body,
                    Err(error) => compaction_error(
                        RelayErrorCode::Internal,
                        &format!("wait for a relay change: {error:#}"),
                    ),
                };
                let response = RelayResponseEnvelope {
                    request_id: envelope.request_id,
                    protocol_version: envelope.protocol_version,
                    body,
                };
                write_logged_response(&mut writer, &response, &session_id, "wait_for_change")
                    .await?;
                continue;
            }
            if let RelayRequest::SetOperatorAway { away, revision } = &envelope.request {
                let state = {
                    let relay = relay.lock().expect("relay state lock poisoned");
                    relay.operator_away.set(mj_core::state::OperatorPresence { away: *away, revision: *revision });
                    relay.operational_state()
                };
                let response = RelayResponseEnvelope {
                    protocol_version: envelope.protocol_version,
                    request_id: envelope.request_id,
                    body: RelayResponseBody::Ok { payload: RelayResponsePayload::Status(state) },
                };
                write_logged_response(&mut writer, &response, &session_id, "set_operator_away").await?;
                continue;
            }
            if matches!(&envelope.request, RelayRequest::BeginLiveBaseline | RelayRequest::ReadLiveBaselinePage { .. } | RelayRequest::AbortLiveBaseline) {
                let cut = if matches!(envelope.request, RelayRequest::BeginLiveBaseline) {
                    let mut releases = relay.releases();
                    let mut receiver = relay.lock().expect("relay state lock poisoned").reserve_live_baseline();
                    // A harness that cannot start ends this worker, which ends
                    // the wait with the connection.
                    let operation = async {
                        let mut previous = None;
                        loop {
                            releases.borrow_and_update();
                            let blockers = relay.observe().expect("relay state lock poisoned").adoption_blockers();
                            if previous.as_ref() != Some(&blockers) {
                                if !blockers.is_empty() {
                                    write_logged_response(&mut writer, &RelayResponseEnvelope {
                                        request_id: envelope.request_id.clone(), protocol_version: envelope.protocol_version,
                                        body: RelayResponseBody::Ok { payload: RelayResponsePayload::LiveBaselineWaiting { blockers: blockers.clone() } },
                                    }, &session_id, "live_baseline_waiting").await?;
                                }
                                previous = Some(blockers);
                            }
                            tokio::select! {
                                biased;
                                cut = &mut receiver => break cut.context("worker dropped its adoption cut")?.map_err(anyhow::Error::msg),
                                changed = releases.changed() => changed.context("worker adoption observation stopped")?,
                            }
                        }
                    };
                    let result: Result<Option<mj_core::relay::LiveSessionBaseline>> = tokio::select! {
                        biased;
                        disconnected = reader.fill_buf() => disconnected.map(|_| None).map_err(anyhow::Error::from),
                        result = operation => result.map(Some),
                    };
                    drop(receiver);
                    relay.lock().expect("relay state lock poisoned").resume_after_adoption_wait()?;
                    let _ = dispatch_wake.try_send(());
                    match result {
                        Ok(Some(cut)) => Some(cut),
                        Ok(None) => return Ok(()),
                        Err(error) => {
                            write_logged_response(&mut writer, &RelayResponseEnvelope {
                                request_id: envelope.request_id.clone(), protocol_version: envelope.protocol_version,
                                body: compaction_error(RelayErrorCode::InvalidState, &format!("{error:#}")),
                            }, &session_id, "live_baseline_refused").await?;
                            continue;
                        }
                    }
                } else { None };
                let request_id = envelope.request_id.clone();
                let protocol_version = envelope.protocol_version;
                let mut download = live_baseline_download.take();
                let (result, download) = mj_core::runtime::spawn_blocking(move || {
                    let result = mj_core::relay::live_baseline_reply(&envelope.request, cut, &mut download, &envelope.request_id, protocol_version);
                    (result, download)
                }).await.context("live recovery baseline download task failed")?;
                live_baseline_download = download;
                let body = match result {
                    Ok(payload) => RelayResponseBody::Ok { payload },
                    Err(error) => compaction_error(RelayErrorCode::InvalidRequest, &format!("{error:#}")),
                };
                write_logged_response(&mut writer, &RelayResponseEnvelope { request_id, protocol_version, body }, &session_id, "live_baseline").await?;
                continue;
            }
            if matches!(
                &envelope.request,
                RelayRequest::AttachmentPresent { .. }
                    | RelayRequest::InstallAttachment { .. }
                    | RelayRequest::ReadAttachment { .. }
                    | RelayRequest::FileAttachmentPresent { .. }
                    | RelayRequest::InstallFileAttachment { .. }
                    | RelayRequest::ReadFileAttachment { .. }
            ) {
                let operation = envelope.request.method_name();
                let request_id = envelope.request_id.clone();
                let protocol_version = envelope.protocol_version;
                let store = mj_core::attachment::AttachmentStore::worker(&relay_root);
                let body =
                    match mj_core::runtime::spawn_blocking(move || -> Result<RelayResponsePayload> {
                        use base64::Engine as _;
                        let base64 = base64::engine::general_purpose::STANDARD;
                        match envelope.request {
                            RelayRequest::AttachmentPresent { reference } => {
                                Ok(RelayResponsePayload::AttachmentPresent {
                                    present: store.contains(&reference)?,
                                })
                            }
                            RelayRequest::InstallAttachment { reference, data } => {
                                anyhow::ensure!(
                                    data.len()
                                        <= mj_core::attachment::MAX_IMAGE_BYTES.div_ceil(3) * 4,
                                    "image upload exceeds 700 KiB"
                                );
                                store.install(&reference, &base64.decode(data)?)?;
                                Ok(RelayResponsePayload::AttachmentInstalled)
                            }
                            RelayRequest::ReadAttachment { reference } => {
                                Ok(RelayResponsePayload::AttachmentData {
                                    data: base64.encode(store.read(&reference)?),
                                })
                            }
                            RelayRequest::FileAttachmentPresent { reference } => {
                                Ok(RelayResponsePayload::AttachmentPresent {
                                    present: store.contains_file(&reference)?,
                                })
                            }
                            RelayRequest::InstallFileAttachment { reference, data } => {
                                anyhow::ensure!(
                                    data.len()
                                        <= mj_core::attachment::MAX_FILE_BYTES.div_ceil(3) * 4,
                                    "file upload exceeds 5 MiB"
                                );
                                store.install_file(&reference, &base64.decode(data)?)?;
                                Ok(RelayResponsePayload::AttachmentInstalled)
                            }
                            RelayRequest::ReadFileAttachment { reference } => {
                                Ok(RelayResponsePayload::AttachmentData {
                                    data: base64.encode(store.read_file(&reference)?),
                                })
                            }
                            _ => unreachable!(),
                        }
                    })
                    .await
                    {
                        Ok(Ok(payload)) => RelayResponseBody::Ok { payload },
                        Ok(Err(error)) => {
                            compaction_error(RelayErrorCode::InvalidRequest, &format!("{error:#}"))
                        }
                        Err(error) => compaction_error(
                            RelayErrorCode::Internal,
                            &format!("attachment task failed: {error}"),
                        ),
                    };
                write_logged_response(
                    &mut writer,
                    &RelayResponseEnvelope {
                        request_id,
                        protocol_version,
                        body,
                    },
                    &session_id,
                    operation,
                )
                .await?;
                continue;
            }
            if matches!(&envelope.request, RelayRequest::CpuUsage) {
                let value = cpu.as_ref().map(|receiver| receiver.borrow().clone()).unwrap_or(Ok(None));
                let body = match value {
                    Ok(usage) => RelayResponseBody::Ok { payload: RelayResponsePayload::CpuUsage { usage } },
                    Err(message) => RelayResponseBody::Error { error: RelayProtocolError {
                        code: RelayErrorCode::Internal, message, retryable: false, detail: None,
                    } },
                };
                let response = RelayResponseEnvelope { request_id: envelope.request_id, protocol_version: envelope.protocol_version, body };
                write_logged_response(&mut writer, &response, &session_id, "cpu_usage").await?;
                continue;
            }
            if matches!(&envelope.request, RelayRequest::ReadRetiredLogins { .. } | RelayRequest::AckRetiredLogin { .. }) {
                // Share the credential admission boundary: Hold sets its state
                // then drains this guard before preservation/commit. An ack can
                // never delete a receipt while a reload is committing it.
                let (read_guard, write_guard) = if matches!(&envelope.request, RelayRequest::AckRetiredLogin { .. }) {
                    (None, Some(credentials.write().await))
                } else {
                    (Some(credentials.read().await), None)
                };
                let held = relay.observe().expect("relay state lock poisoned").in_place_move_hold_active();
                let root = relay_root.clone();
                let request = envelope.request.clone();
                let result = mj_core::runtime::spawn_blocking(move || -> Result<RelayResponsePayload> {
                    if held || super::reload_commit::retired_logins_deferred(&root)? {
                        anyhow::ensure!(matches!(request, RelayRequest::ReadRetiredLogins { .. }), "reload holds retired login acknowledgements");
                        return Ok(RelayResponsePayload::RetiredLogins { logins: Vec::new(), has_more: false, deferred: true });
                    }
                    match request {
                        RelayRequest::ReadRetiredLogins { after } => {
                            let (logins, has_more) = super::reload_commit::read_retired_logins_page(&root, after.as_deref(), mj_core::credentials::RETIRED_LOGIN_PAGE_RAW_BYTES)?;
                            Ok(RelayResponsePayload::RetiredLogins { logins, has_more, deferred: false })
                        },
                        RelayRequest::AckRetiredLogin { id, fingerprint } => {
                            super::reload_commit::ack_retired_login(&root, &id, &fingerprint)?;
                            Ok(RelayResponsePayload::RetiredLoginAcknowledged)
                        }
                        _ => unreachable!(),
                    }
                }).await.context("retired login task stopped").and_then(|result| result);
                drop(read_guard);
                drop(write_guard);
                let body = match result {
                    Ok(payload) => RelayResponseBody::Ok { payload },
                    Err(error) => RelayResponseBody::Error { error: RelayProtocolError {
                        code: RelayErrorCode::Internal, message: format!("retired login operation failed: {error:#}"), retryable: true, detail: None,
                    } },
                };
                write_logged_response(&mut writer, &RelayResponseEnvelope {
                    request_id: envelope.request_id, protocol_version: envelope.protocol_version, body,
                }, &session_id, "retired_login").await?;
                continue;
            }
            if matches!(
                &envelope.request,
                RelayRequest::CredentialState
                    | RelayRequest::AwsCredentialsState
                    | RelayRequest::InstallAwsCredentials { .. }
                    | RelayRequest::RemoveAwsCredentials
                    | RelayRequest::AwsCredentialsScope
                    | RelayRequest::SlopcopLoginState
                    | RelayRequest::InstallSlopcopLogin { .. }
                    | RelayRequest::RemoveSlopcopLogin
                    | RelayRequest::ReadCredentials
                    | RelayRequest::InstallCredentials { .. }
                    | RelayRequest::SkillsState
                    | RelayRequest::InstallSkills { .. }
                    | RelayRequest::GithubTokenState
                    | RelayRequest::InstallGithubToken { .. }
                    | RelayRequest::RemoveGithubToken
            ) {
                // Credential, token, and skills bytes stay on this connection.
                // They never reach DurableRelay, its journal, or its
                // command ledger.
                let operation = envelope.request.method_name();
                let login_installed = matches!(&envelope.request, RelayRequest::InstallCredentials { .. });
                let login_observed = login_installed || matches!(&envelope.request, RelayRequest::CredentialState);
                let response = {
                    // Acquire the read guard before checking `in_place_move_hold`
                    // and hold it through `credential_response`. Because
                    // `HoldInPlaceMoveDispatch` sets the hold before draining
                    // via `credentials.drain().await`, every pre-hold request
                    // finishes before `InPlaceMoveDispatchHeld` replies, and
                    // every post-hold request observes the hold and refuses.
                    let current_credentials = credentials.read().await;
                    let (move_held, login_identity) = {
                        let observed = relay.observe().expect("relay state lock poisoned");
                        (
                            observed.in_place_move_hold_active(),
                            observed.login_identity.clone(),
                        )
                    };
                    if move_held {
                        RelayResponseEnvelope {
                            request_id: envelope.request_id,
                            protocol_version: envelope.protocol_version,
                            body: RelayResponseBody::Error {
                                error: RelayProtocolError {
                                    code: RelayErrorCode::InvalidState,
                                    message: "cannot access credentials while an in-place move dispatch hold is active".into(),
                                    retryable: true,
                                    detail: None,
                                },
                            },
                        }
                    } else {
                        credential_response(
                            envelope,
                            &current_credentials,
                            &relay_root,
                            &login_identity,
                        )
                        .await
                    }
                };
                if login_installed && matches!(response.body, RelayResponseBody::Ok { .. }) {
                    wake_dispatch(&relay, &dispatch_wake)?;
                } else if login_observed {
                    // A failed read can withdraw the previous observation.
                    drop(relay.lock().expect("relay state lock poisoned"));
                }
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if matches!(
                &envelope.request,
                RelayRequest::ProjectMemorySnapshot
                    | RelayRequest::InstallProjectMemorySnapshot { .. }
                    | RelayRequest::ReplaceProjectMemoryTree { .. }
                    | RelayRequest::ProjectMemoryTransfer { .. }
            ) {
                let operation = envelope.request.method_name();
                let cancel = CancellationToken::new();
                let response = project_memory_response(
                    envelope,
                    &project_memory,
                    &mut project_memory_transfer,
                    cancel.clone(),
                );
                tokio::pin!(response);
                // The relay protocol is strictly request-then-response per connection.
                let response = tokio::select! {
                    response = &mut response => response,
                    disconnected = reader.fill_buf() => {
                        // Cancel queued work and reads, but keep ownership of
                        // an admitted write until its filesystem mutation ends.
                        cancel.cancel();
                        #[cfg(test)]
                        project_memory.io_disconnected.notify_one();
                        if let Some(response) = response.await {
                            if let RelayResponseBody::Error { error } = response.body {
                                bail!("project-memory {operation} failed after disconnect: {}", error.message);
                            }
                            tracing::debug!(session_id, operation, "project-memory operation completed after disconnect");
                        }
                        anyhow::ensure!(
                            disconnected?.is_empty(),
                            "protocol error: received a request before the project-memory response",
                        );
                        return Ok(());
                    }
                };
                let Some(response) = response else { return Ok(()) };
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            let guardian_payload = match &envelope.request {
                RelayRequest::GuardianRequests => Some(RelayResponsePayload::GuardianRequests {
                    requests: relay.guardian.requests().iter()
                        .map(|request| request.for_protocol(envelope.protocol_version))
                        .collect(),
                }),
                RelayRequest::CompleteGuardianRequest { result } => {
                    relay.guardian.complete(result.clone());
                    Some(RelayResponsePayload::GuardianRequestCompleted)
                }
                _ => None,
            };
            if let Some(payload) = guardian_payload {
                let operation = envelope.request.method_name();
                let body = RelayResponseBody::Ok { payload };
                write_logged_response(&mut writer, &RelayResponseEnvelope { request_id: envelope.request_id, protocol_version: envelope.protocol_version, body }, &session_id, operation).await?;
                continue;
            }
            if matches!(&envelope.request, RelayRequest::HistoryQuery { .. } | RelayRequest::HistoryRequests | RelayRequest::CompleteHistoryRequest { .. }) {
                let operation = envelope.request.method_name();
                let result: Result<RelayResponsePayload> = match envelope.request {
                    RelayRequest::HistoryRequests => Ok(RelayResponsePayload::HistoryRequests { requests: project_memory.history.requests_for_protocol(envelope.protocol_version) }),
                    RelayRequest::CompleteHistoryRequest { result } => {
                        project_memory.history.complete(result);
                        Ok(RelayResponsePayload::HistoryRequestCompleted)
                    }
                    RelayRequest::HistoryQuery { query } => {
                        if project_memory.config.as_ref().is_none_or(|config| config.history_socket.is_none()) {
                            Err(anyhow::anyhow!("history tools are unavailable in this worker; resume with a current controller"))
                        } else {
                            tokio::select! {
                                result = project_memory.history.query(query) => result.map(|result| RelayResponsePayload::HistoryResult { result }),
                                disconnected = reader.fill_buf() => {
                                    match disconnected {
                                        Ok(_) => return Ok(()),
                                        Err(error) => return Err(error.into()),
                                    }
                                }
                            }
                        }
                    }
                    _ => unreachable!(),
                };
                let body = match result {
                    Ok(payload) => RelayResponseBody::Ok { payload },
                    Err(error) => compaction_error(RelayErrorCode::Internal, &format!("{error:#}")),
                };
                write_logged_response(&mut writer, &RelayResponseEnvelope { request_id: envelope.request_id, protocol_version: envelope.protocol_version, body }, &session_id, operation).await?;
                continue;
            }
            if let RelayRequest::Reviewer { .. } = &envelope.request {
                // Capture operations never enter the conversation journal.
                let operation = envelope.request.method_name();
                let response = if let Some(message) =
                    preparation_error(&prep_snapshot, "reviewer requests")
                {
                    unavailable_request(envelope, message)
                } else if relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .operational_state()
                    .execution
                    == mj_core::relay::RelayExecutionState::Closed
                {
                    unavailable_request(
                        envelope,
                        "session is closed; review capture is unavailable".into(),
                    )
                } else {
                    review_capture_response(envelope, request_reviewer).await
                };
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if matches!(
                &envelope.request,
                RelayRequest::SubagentRequests
                    | RelayRequest::PollSubagentQueue { .. }
                    | RelayRequest::CompleteSubagentRequest { .. }
                    | RelayRequest::SetSubagentAdmission { .. }
            ) {
                let operation = envelope.request.method_name();
                let request_id = envelope.request_id.clone();
                let protocol_version = envelope.protocol_version;
                let unavailable = preparation_error(&prep_snapshot, "sub-agent requests")
                    .filter(|_| has_subagent_tools);
                let body = if let Some(message) = unavailable {
                    compaction_error(RelayErrorCode::InvalidState, &message)
                } else {
                    match (&request_subagents, envelope.request) {
                    (endpoint, RelayRequest::PollSubagentQueue { acknowledged }) => {
                        match subagent_feed.poll(endpoint.as_ref(), acknowledged) {
                            Ok(page) => RelayResponseBody::Ok { payload: RelayResponsePayload::SubagentQueue { page } },
                            Err(error) => compaction_error(RelayErrorCode::Internal, &format!("poll sub-agent queue: {error:#}")),
                        }
                    }
                    (_, RelayRequest::SubagentRequests) => compaction_error(
                        RelayErrorCode::InvalidRequest,
                        "this worker uses poll_subagent_queue; historical reply snapshots are no longer served",
                    ),
                    (Some(endpoint), RelayRequest::CompleteSubagentRequest { result }) => {
                        match endpoint.complete(result) {
                            Ok(delivered_to_waiter) => RelayResponseBody::Ok {
                                payload: if protocol_version
                                    >= mj_core::relay::RELAY_SUBAGENT_DELIVERY_PROTOCOL
                                {
                                    RelayResponsePayload::SubagentRequestCompletedWithDelivery {
                                        delivered_to_waiter,
                                    }
                                } else {
                                    RelayResponsePayload::SubagentRequestCompleted
                                },
                            },
                            Err(error) => compaction_error(
                                RelayErrorCode::Internal,
                                &format!("persist sub-agent result: {error:#}"),
                            ),
                        }
                    }
                    (Some(endpoint), RelayRequest::SetSubagentAdmission { open }) => {
                        match endpoint.set_mutating_admission(open) {
                            Ok(()) => RelayResponseBody::Ok {
                                payload: RelayResponsePayload::SubagentAdmissionChanged { open },
                            },
                            Err(error) => compaction_error(
                                RelayErrorCode::Internal,
                                &format!("change sub-agent admission: {error:#}"),
                            ),
                        }
                    }
                    (None, RelayRequest::CompleteSubagentRequest { .. }) => compaction_error(
                        RelayErrorCode::InvalidRequest,
                        "this session has no Mjolnir sub-agent tools",
                    ),
                    (None, RelayRequest::SetSubagentAdmission { .. }) => compaction_error(
                        RelayErrorCode::InvalidRequest,
                        "this session has no Mjolnir sub-agent tools",
                    ),
                    _ => unreachable!(),
                    }
                };
                write_logged_response(
                    &mut writer,
                    &RelayResponseEnvelope {
                        request_id,
                        protocol_version,
                        body,
                    },
                    &session_id,
                    operation,
                )
                .await?;
                continue;
            }
            if let RelayRequest::RespondElicitation { .. } = &envelope.request {
                // Form answers can contain private user input. They travel
                // directly to the ACP runtime and never touch relay state.
                let response = if let Some(message) =
                    preparation_error(&prep_snapshot, "elicitation responses")
                {
                    unavailable_request(envelope, message)
                } else {
                    elicitation_response(envelope, request_commands.as_ref()).await
                };
                write_logged_response(&mut writer, &response, &session_id, "respond_elicitation")
                    .await?;
                continue;
            }
            if let RelayRequest::StopBackgroundTask { .. } = &envelope.request {
                let response = if let Some(message) =
                    preparation_error(&prep_snapshot, "background-task controls")
                {
                    unavailable_request(envelope, message)
                } else {
                    background_task_stop_response(envelope, request_commands.as_ref(), &relay).await
                };
                write_logged_response(&mut writer, &response, &session_id, "stop_background_task")
                    .await?;
                continue;
            }
            if let RelayRequest::RestartHarnessInPlace { .. } = &envelope.request {
                let response = if let Some(message) =
                    preparation_error(&prep_snapshot, "in-place harness restart")
                {
                    unavailable_request(envelope, message)
                } else {
                    restart_harness_in_place_response(
                        envelope,
                        request_commands.as_ref(),
                        &relay,
                        &credentials,
                        &relay_root,
                        &shared_subagents,
                        preparation_tx.as_ref(),
                    )
                    .await
                };
                write_logged_response(
                    &mut writer,
                    &response,
                    &session_id,
                    "restart_harness_in_place",
                )
                .await?;
                continue;
            }
            let wakes_dispatch = matches!(
                &envelope.request,
                RelayRequest::Submit { .. }
                    | RelayRequest::ReserveIdle { .. }
                    | RelayRequest::ReleaseInPlaceMoveDispatch { .. }
            );
            let hook_activity_changed = matches!(
                &envelope.request,
                RelayRequest::DrainMailbox { .. }
                    | RelayRequest::AckMailbox { .. }
                    | RelayRequest::AntigravityStop { .. }
            );
            let checkpoint_change = checkpoint_change(&envelope.request);
            let operation = envelope.request.method_name();
            // An attach's state carries the preparation before the relay
            // digests it, so a changed preparation is never mistaken for a
            // state the controller already holds.
            let attach_preparation = preparation
                .as_ref()
                .and_then(|receiver| receiver.borrow().state.clone());
            let reservation_response = if let RelayRequest::ReserveIdle { command_id } = &envelope.request {
                use crate::relay::ReserveIdleAntigravityCheck;
                let immediate = {
                    let mut state = relay.lock().expect("relay state lock poisoned");
                    if let Some(body) = state.envelope_rejection(&envelope) {
                        Some(RelayResponseEnvelope { request_id: envelope.request_id.clone(), protocol_version: envelope.protocol_version, body })
                    } else { match state.reserve_idle_antigravity_check(command_id) {
                        ReserveIdleAntigravityCheck::ProceedDirectly => Some(state.handle(envelope.clone())),
                        ReserveIdleAntigravityCheck::RejectNow => Some(state.reserve_idle_after_antigravity_refresh(envelope.clone(), None)),
                        ReserveIdleAntigravityCheck::RefreshFirst => None,
                    } }
                };
                if let Some(response) = immediate { Some(response) } else {
                    let synced_epoch = if let Some(tx) = &antigravity_refresh {
                        let (reply, result) = oneshot::channel();
                        if tx.send(reply).await.is_ok() { result.await.ok().flatten() } else { None }
                    } else { None };
                    Some(relay.lock().expect("relay state lock poisoned")
                        .reserve_idle_after_antigravity_refresh(envelope.clone(), synced_epoch))
                }
            } else { None };
            let result = if let Some(response) = reservation_response { Ok(response) }
            else if matches!(envelope.request, RelayRequest::AntigravityStop { .. }) {
                Ok(handle_antigravity_stop(&relay, envelope, &credentials).await)
            } else {
                handle_request(&relay, envelope, attach_preparation).await
            };
            let mut response = match result {
                Ok(response) => response,
                Err(error) => {
                    tracing::error!(
                        %session_id,
                        %operation,
                        "relay request handling failed: {error:#}"
                    );
                    return Err(error);
                }
            };
            if matches!(
                response.body,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::InPlaceMoveDispatchHeld { .. }
                }
            ) {
                // Drain every credential RPC that acquired a read guard before
                // the hold was recorded, before telling the controller it may
                // harvest or overwrite staged profile files.
                credentials.drain().await;
            }
            // One relay lock decides who owns the relay-local lifecycle work
            // this request may have queued. Until preparation publishes
            // Started, which `start_bridge` does under this same lock, this
            // handler completes checkpoint and Close commands itself, and a
            // Close that sealed the relay cancels preparation so no bridge
            // starts. Once Started is visible here, the coordinator owns them.
            let current_preparation = {
                let mut state = relay.lock().expect("relay state lock poisoned");
                let current = preparation
                    .as_ref()
                    .map(|receiver| receiver.borrow().clone())
                    .unwrap_or_default();
                if preparation.is_some() && current.is_pending() {
                    state.dispatch_preparation_lifecycle()?;
                    if state.sealed_by_close()
                        && let Some(cancel) = &preparation_cancel
                    {
                        cancel.cancel();
                    }
                }
                current
            };
            if worker_root_was_removed(&response.body, &relay_root) {
                // One report is enough; the daemon is already winding down.
                report_fatal(
                    &fatal,
                    anyhow::anyhow!(
                        "worker root {} was removed while the relay was serving",
                        relay_root.display()
                    ),
                    &session_id,
                    "worker root removed",
                );
            }
            let accepted = matches!(
                &response.body,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::Accepted { .. }
                        | RelayResponsePayload::IdleReservation { ordinal: Some(_) }
                        | RelayResponsePayload::InPlaceMoveDispatchReleased
                }
            );
            if accepted {
                match checkpoint_change {
                    Some(CheckpointChange::Begin(command_id)) => {
                        checkpoint_barriers.insert(command_id);
                    }
                    Some(CheckpointChange::Ended(command_id)) => {
                        checkpoint_barriers.remove(&command_id);
                    }
                    None => {}
                }
            }
            if (wakes_dispatch && accepted) || hook_activity_changed {
                wake_dispatch(&relay, &dispatch_wake)?;
            }
            overlay_preparation_state(&mut response, &current_preparation);
            // Dispatch is driven from durable state, not from delivery of
            // the acknowledgement. A controller can disappear after its
            // request reaches the relay but before the response write.
            write_logged_response(&mut writer, &response, &session_id, operation).await?;
        }
        Ok(())
    }
    .await;

    let cleanup_result = release_checkpoint_barriers(&relay, &dispatch_wake, checkpoint_barriers);
    let outcome = match (serving_result, cleanup_result) {
        (Ok(()), cleanup_result) => cleanup_result,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "also failed to release checkpoint barriers: {cleanup_error:#}"
        ))),
    };
    if outcome.is_err() && !relay_root.is_dir() {
        report_fatal(
            &fatal,
            anyhow::anyhow!(
                "worker root {} was removed while the relay was serving",
                relay_root.display()
            ),
            &session_id,
            "worker root removed",
        );
    }
    outcome
}

#[cfg(test)]
mod preparation_tests {
    use super::*;
    use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
    use mj_core::relay::{RELAY_EVENT_GENESIS_DIGEST, RelayCursor, RelayVersionRange};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    type TestReader = BufReader<OwnedReadHalf>;

    fn test_relay(root: &std::path::Path) -> Arc<RelayCell> {
        Arc::new(RelayCell::new(
            DurableRelay::open(root, "preparation-test", "1.0.0").unwrap(),
        ))
    }

    async fn start_control_server(
        root: &std::path::Path,
        relay: Arc<RelayCell>,
        dispatch_wake: mpsc::Sender<()>,
        preparation: watch::Receiver<PreparationSnapshot>,
        preparation_cancel: CancellationToken,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        TestReader,
        OwnedWriteHalf,
    ) {
        let socket = root.join("control.sock");
        let (listener, guard) = publish_control_socket(root, &socket).unwrap();
        let (fatal, _fatal_reports) = mpsc::channel(1);
        let accept = mj_core::runtime::spawn(accept_worker_clients(
            listener,
            guard,
            relay,
            dispatch_wake,
            SharedCredentials::new(Err("fixture has no credentials".into())),
            ConnectionRuntime {
                preparation: Some(preparation),
                preparation_cancel: Some(preparation_cancel),
                ..Default::default()
            },
            fatal,
        ));
        let client = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(socket))
            .await
            .unwrap()
            .unwrap();
        let (read, write) = client.into_split();
        (accept, BufReader::new(read), write)
    }

    async fn request(
        reader: &mut TestReader,
        writer: &mut OwnedWriteHalf,
        request_id: &str,
        request: RelayRequest,
    ) -> RelayResponseEnvelope {
        let mut encoded = serde_json::to_vec(&RelayRequestEnvelope {
            request_id: request_id.to_owned(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request,
        })
        .unwrap();
        encoded.push(b'\n');
        writer.write_all(&encoded).await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn adoption_wait_captures_between_turns_and_disconnect_releases_queued_work() {
        use base64::Engine as _;
        use mj_core::relay::{LiveSessionBaseline, RelayCommandOutcome};
        for mode in ["served", "disconnect", "another-waiter"] {
            let temp = tempfile::tempdir().unwrap();
            let relay = test_relay(temp.path());
            let (wake, _wakes) = mpsc::channel(1);
            let (_status, preparation) = watch::channel(PreparationSnapshot::default());
            let (accept, mut reader, mut writer) = start_control_server(
                temp.path(),
                relay.clone(),
                wake,
                preparation,
                CancellationToken::new(),
            )
            .await;
            request(
                &mut reader,
                &mut writer,
                "hello",
                RelayRequest::Hello {
                    controller_version: "test".into(),
                    supported: RelayVersionRange::CURRENT,
                },
            )
            .await;
            for (id, command) in [
                (
                    "running-prompt",
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::from("first")],
                    },
                ),
                (
                    "next-prompt",
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::from("second")],
                    },
                ),
                (
                    "queued-shell",
                    RelayCommand::RunUserShell {
                        command: "printf next".into(),
                    },
                ),
            ] {
                let response = request(
                    &mut reader,
                    &mut writer,
                    id,
                    RelayRequest::Submit {
                        command_id: id.into(),
                        command,
                    },
                )
                .await;
                assert!(
                    matches!(response.body, RelayResponseBody::Ok { .. }),
                    "{response:?}"
                );
            }
            relay.lock().unwrap().claim_pending_commands(true).unwrap();
            let response = request(
                &mut reader,
                &mut writer,
                "cut",
                RelayRequest::BeginLiveBaseline,
            )
            .await;
            assert!(matches!(
                response.body,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::LiveBaselineWaiting { .. }
                }
            ));
            assert!(
                relay
                    .lock()
                    .unwrap()
                    .claim_pending_user_shell_commands_up_to(1)
                    .unwrap()
                    .is_empty()
            );
            if mode == "another-waiter" {
                let peer = UnixStream::connect(temp.path().join("control.sock"))
                    .await
                    .unwrap();
                let (read, write) = peer.into_split();
                let mut other_reader = BufReader::new(read);
                let mut other_writer = write;
                request(
                    &mut other_reader,
                    &mut other_writer,
                    "hello-other",
                    RelayRequest::Hello {
                        controller_version: "other".into(),
                        supported: RelayVersionRange::CURRENT,
                    },
                )
                .await;
                let other = request(
                    &mut other_reader,
                    &mut other_writer,
                    "cut-other",
                    RelayRequest::BeginLiveBaseline,
                )
                .await;
                assert!(matches!(
                    other.body,
                    RelayResponseBody::Ok {
                        payload: RelayResponsePayload::LiveBaselineWaiting { .. }
                    }
                ));
                drop(reader);
                drop(writer);
                reader = other_reader;
                writer = other_writer;
                assert!(
                    relay
                        .lock()
                        .unwrap()
                        .claim_pending_user_shell_commands_up_to(1)
                        .unwrap()
                        .is_empty()
                );
            }
            if mode == "disconnect" {
                drop(writer);
                drop(reader);
                // The current turn still runs: EOF alone must remove the hold.
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if !relay
                            .lock()
                            .unwrap()
                            .claim_pending_user_shell_commands_up_to(1)
                            .unwrap()
                            .is_empty()
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            } else {
                relay
                    .lock()
                    .unwrap()
                    .record_command_completed(
                        "running-prompt",
                        RelayCommandOutcome::Prompt {
                            stop_reason: "end_turn".into(),
                            diagnostic: None,
                            usage: None,
                            work_facts: None,
                        },
                    )
                    .unwrap();
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                let response: RelayResponseEnvelope = serde_json::from_str(&line).unwrap();
                assert!(matches!(
                    response.body,
                    RelayResponseBody::Ok {
                        payload: RelayResponsePayload::LiveBaselineStarted { .. }
                    }
                ));
                let mut bytes = Vec::new();
                let mut offset = 0;
                loop {
                    let response = request(
                        &mut reader,
                        &mut writer,
                        "page",
                        RelayRequest::ReadLiveBaselinePage { offset },
                    )
                    .await;
                    let RelayResponseBody::Ok {
                        payload: RelayResponsePayload::LiveBaselinePage { page },
                    } = response.body
                    else {
                        panic!("{response:?}")
                    };
                    bytes.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(&page.chunk)
                            .unwrap(),
                    );
                    offset = page.next_offset;
                    if page.complete {
                        break;
                    }
                }
                let cut: LiveSessionBaseline = serde_json::from_slice(&bytes).unwrap();
                assert!(cut.state.active_prompt.is_none());
                assert_eq!(cut.queue[0].command_id, "next-prompt");
                assert_eq!(cut.state.active_user_shells[0].command_id, "queued-shell");
                assert!(cut.state.active_user_shells[0].started_at_ms.is_none());
                relay.lock().unwrap().claim_pending_commands(true).unwrap();
                assert_eq!(
                    relay
                        .lock()
                        .unwrap()
                        .operational_state()
                        .active_prompt
                        .unwrap()
                        .command_id,
                    "next-prompt"
                );
                assert!(relay.lock().unwrap().latest_ordinal() > cut.state.latest_ordinal);
                drop(writer);
                drop(reader);
            }
            accept.abort();
            let _ = accept.await;
        }
    }

    #[tokio::test]
    async fn failing_project_recipe_records_notice_and_continues_preparation() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let checkout = root.join("checkout");
        let bin = root.join("bin");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(checkout.join("mise.toml"), "[tools]\nmissing = '1'\n").unwrap();
        let mise = bin.join("mise");
        // More than 64KB catches pipe truncation/deadlocks; both streams belong
        // in the log even when installation exits unsuccessfully.
        std::fs::write(&mise, "#!/bin/sh\n/usr/bin/head -c 131072 /dev/zero\nprintf 'recipe failed\\n' >&2\nexit 42\n").unwrap();
        std::fs::set_permissions(&mise, std::fs::Permissions::from_mode(0o755)).unwrap();
        let relay = test_relay(root);
        let (status, _) = watch::channel(PreparationSnapshot::default());
        let mut config = super::super::relay_tests::launch_config(root.to_str().unwrap());
        config
            .environment
            .insert("PATH".into(), bin.to_string_lossy().into_owned());
        config.environment.insert(
            "MISE_DATA_DIR".into(),
            root.join("mise").to_string_lossy().into_owned(),
        );
        let (_, install) = prepare_project_tools(
            root,
            &[checkout],
            &mut config,
            &BTreeMap::new(),
            Some(&status),
            &relay,
        )
        .await
        .unwrap();
        // mise failing its status commands falls back to naming the recipe
        // files; the installation then runs after preparation returns.
        install
            .expect("installation planned")
            .run(root, &relay)
            .await;
        let mut journal = relay.lock().unwrap();
        let mj_core::relay::RelayResponsePayload::MailboxDrained { text, count, .. } =
            journal.drain_mailbox("PostToolBatch", None).unwrap()
        else {
            panic!("drain returned another payload")
        };
        let text = text.unwrap();
        assert_eq!(count, 2, "{text}");
        let started = text.find("being installed in the background").unwrap();
        let finished = text.find("did not finish").unwrap();
        assert!(started < finished, "{text}");
        assert!(text.contains("mise.toml") && text.contains("second message"));
        assert!(
            text.contains("42")
                && text.contains("project-tools.log")
                && text.contains("rerun `mise install`")
        );
        let events = journal.events_after(0, RELAY_EVENT_GENESIS_DIGEST).unwrap();
        let notices = events
            .iter()
            .filter_map(|event| match &event.observation {
                RelayObservation::Notice { message } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 2);
        assert!(
            notices
                .iter()
                .any(|message| message.contains("mise install")
                    && message.contains("42")
                    && message.contains("project-tools.log"))
        );
        let log = std::fs::read(root.join(super::super::project_tools::LOG_FILE)).unwrap();
        assert!(log.len() > 262144);
        assert!(String::from_utf8_lossy(&log).contains("recipe failed"));
    }

    #[tokio::test]
    async fn hello_attach_and_prompt_queue_work_while_fake_preparation_is_blocked() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let relay = test_relay(root);
        let (status, preparation) = watch::channel(PreparationSnapshot::default());
        let cancellation = CancellationToken::new();
        let (dispatch_wake, dispatch_wakes) = mpsc::channel(1);
        let (commands, mut command_requests) = mpsc::channel(8);
        let prepared = PreparationSnapshot {
            state: Some(mj_core::relay::HarnessPreparation::Started),
            services: Some(Arc::new(PreparedConnectionServices {
                commands: commands.clone(),
                reviewer: Arc::new(ReviewCapture {
                    session_id: "preparation-test".into(),
                    cwd: root.into(),
                    additional_directories: Vec::new(),
                    primary_relay: relay.clone(),
                    operation: tokio::sync::Mutex::new(()),
                }),
                subagents: None,
            })),
            ..Default::default()
        };
        let (preparation_started, started) = tokio::sync::oneshot::channel();
        let (release_preparation, release) = tokio::sync::oneshot::channel();
        let prep_root = root.to_path_buf();
        let prep_status = status.clone();
        let fake_root = prep_root.clone();
        let fake_status = prep_status.clone();
        let prep_cancellation = cancellation.clone();
        let fake_cancellation = prep_cancellation.clone();
        let prep = mj_core::runtime::spawn(async move {
            supervise_preparation(
                &prep_root,
                &Arc::default(),
                &prep_status,
                &prep_cancellation,
                async move {
                    preparation_step(
                        &fake_root,
                        Some(&fake_status),
                        "harness-resolve",
                        LOCAL_PREPARATION_STEP_TIMEOUT,
                        &fake_cancellation,
                    )
                    .await?;
                    let _ = preparation_started.send(());
                    release.await.context("fake preparation gate was dropped")?;
                    fake_status.send_replace(prepared);
                    Ok(())
                },
            )
            .await
        });
        started.await.unwrap();
        let (accept, mut reader, mut writer) = start_control_server(
            root,
            relay.clone(),
            dispatch_wake.clone(),
            preparation,
            cancellation,
        )
        .await;

        let hello = request(
            &mut reader,
            &mut writer,
            "hello",
            RelayRequest::Hello {
                controller_version: "test".into(),
                supported: RelayVersionRange::CURRENT,
            },
        )
        .await;
        assert!(matches!(
            hello.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Hello { .. }
            }
        ));
        let attached = request(
            &mut reader,
            &mut writer,
            "attach",
            RelayRequest::Attach {
                after_ordinal: 0,
                after_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
                known_state_digest: None,
            },
        )
        .await;
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::Attached { state, .. },
        } = attached.body
        else {
            panic!(
                "attach failed before preparation completed: {:?}",
                attached.body
            );
        };
        assert!(matches!(
            state.harness_preparation,
            Some(mj_core::relay::HarnessPreparation::Preparing { ref step, .. })
                if step == "harness-resolve"
        ));

        let submitted = request(
            &mut reader,
            &mut writer,
            "prompt-submit",
            RelayRequest::Submit {
                command_id: "prompt-during-preparation".into(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("queued prompt"))],
                },
            },
        )
        .await;
        assert!(matches!(submitted.body, RelayResponseBody::Ok { .. }));
        let status_response = request(
            &mut reader,
            &mut writer,
            "status-queued",
            RelayRequest::Status,
        )
        .await;
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::Status(state),
        } = status_response.body
        else {
            panic!("status failed while preparation was blocked");
        };
        assert!(
            state
                .active_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.command_id == "prompt-during-preparation")
                || state
                    .queued_prompts
                    .iter()
                    .any(|prompt| prompt.command_id == "prompt-during-preparation"),
            "accepted prompt was not represented in relay state: {state:?}"
        );
        assert!(command_requests.try_recv().is_err());

        release_preparation.send(()).unwrap();
        prep.await.unwrap().unwrap();
        let (events, event_stream) = mpsc::channel(16);
        let (shell_events, _shell_event_stream) = mpsc::channel(1);
        let shells = crate::user_shell::UserShellRegistry::new(
            root.to_path_buf(),
            BTreeMap::new(),
            shell_events,
        );
        let coordinator = mj_core::runtime::spawn(run_relay_coordinator_with_verdict(
            relay,
            event_stream,
            dispatch_wakes,
            commands,
            shells,
            None,
            None,
            None,
        ));
        events
            .send(RuntimeEvent::SessionConfigured {
                config_options: Vec::new(),
            })
            .await
            .unwrap();
        let command = tokio::time::timeout(Duration::from_secs(2), command_requests.recv())
            .await
            .unwrap();
        assert!(
            matches!(command, Some(CommandRequest::Prompt { request_id, .. }) if request_id == "prompt-during-preparation")
        );

        coordinator.abort();
        let _ = coordinator.await;
        writer.shutdown().await.unwrap();
        accept.abort();
    }

    /// The typed failure a stopped preparation ends with, which becomes the
    /// worker's exit record.
    fn startup_failure(stop: PreparationStop) -> mj_core::relay::WorkerHarnessStartupFailure {
        let PreparationStop::Failed(error) = stop else {
            panic!("preparation was cancelled instead of failing");
        };
        error
            .downcast_ref::<mj_core::relay::WorkerHarnessStartupFailure>()
            .unwrap_or_else(|| panic!("untyped preparation failure: {error:#}"))
            .clone()
    }

    #[tokio::test]
    async fn panicking_preparation_ends_with_a_typed_startup_failure() {
        fn panic_fake_preparation() -> Result<()> {
            panic!("fake profile step panicked");
        }

        let temp = tempfile::tempdir().unwrap();
        let (status, _preparation) = watch::channel(PreparationSnapshot {
            state: Some(mj_core::relay::HarnessPreparation::Preparing {
                step: "subagent-profile".into(),
                since_ms: chrono::Utc::now().timestamp_millis(),
            }),
            ..Default::default()
        });
        let cancellation = CancellationToken::new();
        let result = supervise_preparation::<()>(
            temp.path(),
            &Arc::default(),
            &status,
            &cancellation,
            async { panic_fake_preparation() },
        )
        .await;
        let failure = startup_failure(result.unwrap_err());
        assert!(
            failure
                .message
                .starts_with("harness preparation failed at subagent-profile: ")
                && failure.message.contains("fake profile step panicked"),
            "{failure:?}"
        );
        assert!(matches!(
            status.borrow().state,
            Some(mj_core::relay::HarnessPreparation::Preparing { .. })
        ));
    }

    // A step whose thread blocks past its deadline fails the preparation at
    // once: the worker exits, which is the only way to end that thread.
    #[tokio::test]
    async fn blocking_preparation_deadline_fails_without_waiting_for_its_thread() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (status, _preparation) = watch::channel(PreparationSnapshot::default());
        let cancellation = CancellationToken::new();
        let step_status = status.clone();
        let step_cancellation = cancellation.clone();
        let (release_blocking_tx, release_blocking_rx) = std::sync::mpsc::channel::<()>();
        let (blocking_finished_tx, blocking_finished_rx) = std::sync::mpsc::channel();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            supervise_preparation::<()>(
                &root,
                &Arc::default(),
                &status,
                &cancellation,
                async move {
                    // Set the step snapshot and budget directly without running
                    // `preparation_step`'s startup-breadcrumb disk write inside the
                    // 200ms budget, so heavy parallel load cannot expire the step
                    // before the blocked closure is spawned.
                    let budget =
                        PreparationStepBudget::new("acp-setup", Duration::from_millis(200));
                    step_status.send_modify(|snapshot| {
                        snapshot.state = Some(mj_core::relay::HarnessPreparation::Preparing {
                            step: "acp-setup".to_owned(),
                            since_ms: chrono::Utc::now().timestamp_millis(),
                        });
                        snapshot.step_deadline = Some(budget.deadline);
                    });
                    bounded_blocking_preparation_step(&budget, &step_cancellation, move |_| {
                        let _ = release_blocking_rx.recv();
                        let _ = blocking_finished_tx.send(());
                        Ok(())
                    })
                    .await
                },
            ),
        )
        .await
        .expect("an expired step must not wait for its blocked thread");
        let failure = startup_failure(result.unwrap_err());
        assert!(
            failure.message.contains("acp-setup") && failure.message.contains("200ms deadline"),
            "{failure:?}"
        );
        assert!(matches!(
            blocking_finished_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        let startup: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(super::super::WORKER_STARTUP_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(startup["failure"]["step"], "acp-setup");
        release_blocking_tx.send(()).unwrap();
        mj_core::runtime::spawn_blocking(move || blocking_finished_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }

    // A Close accepted during preparation and the Started publication are
    // decided under the relay lock, in one order or the other: a sealed relay
    // never gets a bridge, and a started bridge's coordinator owns lifecycle
    // work the handler then leaves alone. A seal that `publish_started` finds
    // before the handler has cancelled preparation is still a cancellation,
    // not a startup failure that would end the worker mid-Close.
    // Hard-won: #1617: a Close racing Started cancelled a coordinator that held it InFlight.
    #[tokio::test]
    async fn a_close_during_preparation_and_started_have_one_order() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Order {
            CloseThroughHandler,
            StartedFirst,
            SealedBeforeHandlerCancels,
        }
        for order in [
            Order::CloseThroughHandler,
            Order::StartedFirst,
            Order::SealedBeforeHandlerCancels,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let relay = test_relay(root);
            let (status, preparation) = watch::channel(PreparationSnapshot {
                state: Some(mj_core::relay::HarnessPreparation::Preparing {
                    step: "acp-setup".into(),
                    since_ms: 1,
                }),
                ..Default::default()
            });
            let cancellation = CancellationToken::new();
            let (dispatch_wake, dispatch_wakes) = mpsc::channel(1);
            let mut dispatch_wakes = Some(dispatch_wakes);
            let (commands, _command_requests) = mpsc::channel(1);
            let services = || PreparedConnectionServices {
                commands: commands.clone(),
                reviewer: Arc::new(ReviewCapture {
                    session_id: "preparation-test".into(),
                    cwd: root.into(),
                    additional_directories: Vec::new(),
                    primary_relay: relay.clone(),
                    operation: tokio::sync::Mutex::new(()),
                }),
                subagents: None,
            };
            let (accept, mut reader, mut writer) = start_control_server(
                root,
                relay.clone(),
                dispatch_wake,
                preparation,
                cancellation.clone(),
            )
            .await;
            // Held as the coordinator would hold it.
            let _coordinator_wakes = (order == Order::StartedFirst).then(|| {
                publish_started(
                    &relay,
                    &cancellation,
                    &status,
                    &mut dispatch_wakes,
                    services(),
                )
                .unwrap()
            });
            let checkpoint = request(
                &mut reader,
                &mut writer,
                "checkpoint-before-close",
                RelayRequest::Submit {
                    command_id: "close-barrier".into(),
                    command: RelayCommand::BeginCheckpoint { reason: None },
                },
            )
            .await;
            assert!(matches!(checkpoint.body, RelayResponseBody::Ok { .. }));
            let RelayResponseBody::Ok {
                payload: RelayResponsePayload::Status(state),
            } = request(&mut reader, &mut writer, "cursor", RelayRequest::Status)
                .await
                .body
            else {
                panic!("status unavailable during preparation");
            };
            if order == Order::StartedFirst {
                assert!(
                    state.checkpoint_ready.is_none(),
                    "the handler completed a checkpoint the coordinator owns"
                );
                assert!(!cancellation.is_cancelled());
                writer.shutdown().await.unwrap();
                accept.abort();
                continue;
            }
            let close = RelayRequest::Submit {
                command_id: "close-during-preparation".into(),
                command: RelayCommand::Close {
                    barrier_command_id: "close-barrier".into(),
                    expected: state.checkpoint_ready.unwrap(),
                },
            };
            if order == Order::CloseThroughHandler {
                let closed = request(&mut reader, &mut writer, "close", close).await;
                assert!(matches!(closed.body, RelayResponseBody::Ok { .. }));
                assert!(cancellation.is_cancelled());
            } else {
                // Accepted, as by a handler that has not yet taken the lock
                // again to cancel preparation.
                let closed = relay.lock().unwrap().handle(RelayRequestEnvelope {
                    protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
                    request_id: "close".into(),
                    request: close,
                });
                assert!(matches!(closed.body, RelayResponseBody::Ok { .. }));
                assert!(!cancellation.is_cancelled());
            }
            let fake_relay = relay.clone();
            let fake_cancellation = cancellation.clone();
            let fake_status = status.clone();
            let fake_wakes = &mut dispatch_wakes;
            let fake_services = services();
            let stop = supervise_preparation(
                root,
                &relay.preparation_writer,
                &status,
                &cancellation,
                async move {
                    publish_started(
                        &fake_relay,
                        &fake_cancellation,
                        &fake_status,
                        fake_wakes,
                        fake_services,
                    )
                    .map(drop)
                },
            )
            .await
            .expect_err("a bridge started for a relay a Close had sealed");
            assert!(
                matches!(stop, PreparationStop::Cancelled),
                "{order:?}: {stop:?}"
            );
            assert!(status.borrow().is_pending());
            assert!(
                dispatch_wakes.is_some(),
                "the sealed relay lost its dispatch wakes"
            );
            writer.shutdown().await.unwrap();
            accept.abort();
        }
    }

    #[tokio::test]
    async fn close_during_preparation_kills_the_fake_harness_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let relay = test_relay(root);
        let (status, preparation) = watch::channel(PreparationSnapshot::default());
        let cancellation = CancellationToken::new();
        let (dispatch_wake, dispatch_wakes) = mpsc::channel(1);
        let (accept, mut reader, mut writer) = start_control_server(
            root,
            relay,
            dispatch_wake,
            preparation,
            cancellation.clone(),
        )
        .await;
        let pid_path = root.join("fake-harness.pid");
        let prep_root = root.to_path_buf();
        let prep_status = status.clone();
        let fake_root = prep_root.clone();
        let fake_status = prep_status.clone();
        let fake_pid_path = pid_path.clone();
        let prep_cancellation = cancellation.clone();
        let fake_cancellation = prep_cancellation.clone();
        let prep = mj_core::runtime::spawn(async move {
            supervise_preparation::<()>(
                &prep_root,
                &Arc::default(),
                &prep_status,
                &prep_cancellation,
                async move {
                    preparation_step(
                        &fake_root,
                        Some(&fake_status),
                        "harness-resolve",
                        LOCAL_PREPARATION_STEP_TIMEOUT,
                        &fake_cancellation,
                    )
                    .await?;
                    let mut child = tokio::process::Command::new("sh");
                    child
                        .arg("-c")
                        .arg("printf '%s\\n' \"$$\" > \"$FAKE_PID_FILE\"; exec sleep 60")
                        .env("FAKE_PID_FILE", &fake_pid_path);
                    let output = mj_core::subprocess::run_bounded(
                        &mut child,
                        1024,
                        Duration::from_secs(120),
                    )
                    .await?;
                    anyhow::bail!("fake harness unexpectedly exited: {}", output.status)
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pid_path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake harness process started");
        let pid: i32 = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let checkpoint = request(
            &mut reader,
            &mut writer,
            "checkpoint-before-close",
            RelayRequest::Submit {
                command_id: "close-barrier".into(),
                command: RelayCommand::BeginCheckpoint { reason: None },
            },
        )
        .await;
        assert!(matches!(checkpoint.body, RelayResponseBody::Ok { .. }));
        let state = match request(
            &mut reader,
            &mut writer,
            "checkpoint-cursor",
            RelayRequest::Status,
        )
        .await
        .body
        {
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Status(state),
            } => state,
            body => panic!("checkpoint status unavailable: {body:?}"),
        };
        let cursor: RelayCursor = state.checkpoint_ready.unwrap();
        let closed = request(
            &mut reader,
            &mut writer,
            "close-during-preparation",
            RelayRequest::Submit {
                command_id: "close-during-preparation".into(),
                command: RelayCommand::Close {
                    barrier_command_id: "close-barrier".into(),
                    expected: cursor,
                },
            },
        )
        .await;
        assert!(
            matches!(closed.body, RelayResponseBody::Ok { .. }),
            "close did not complete during preparation: {:?}",
            closed.body
        );
        let completed = request(
            &mut reader,
            &mut writer,
            "complete-checkpoint",
            RelayRequest::Submit {
                command_id: "checkpoint-complete".into(),
                command: RelayCommand::CompleteCheckpoint {
                    barrier_command_id: "close-barrier".into(),
                },
            },
        )
        .await;
        assert!(matches!(completed.body, RelayResponseBody::Ok { .. }));
        let preparation_result = tokio::time::timeout(Duration::from_secs(2), prep)
            .await
            .expect("close cancels the preparation owner")
            .unwrap();
        assert!(matches!(
            preparation_result,
            Err(PreparationStop::Cancelled)
        ));
        assert!(matches!(
            status.borrow().state.as_ref(),
            Some(mj_core::relay::HarnessPreparation::Preparing { step, .. })
                if step == "harness-resolve"
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                // SAFETY: signal zero only queries whether the fixture PID still exists.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("preparation cancellation terminates its process group");
        let final_state = match request(
            &mut reader,
            &mut writer,
            "closed-status",
            RelayRequest::Status,
        )
        .await
        .body
        {
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Status(state),
            } => state,
            body => panic!("closed relay stopped serving: {body:?}"),
        };
        assert_eq!(
            final_state.execution,
            mj_core::relay::RelayExecutionState::Closed
        );
        // A sealed relay is not a startup failure; Closed is the fact.
        assert!(!matches!(
            final_state.harness_preparation,
            Some(mj_core::relay::HarnessPreparation::Failed { .. })
        ));
        drop(dispatch_wakes);
        writer.shutdown().await.unwrap();
        accept.abort();
    }

    #[tokio::test]
    async fn releasing_in_place_move_hold_wakes_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let relay = test_relay(temp.path());
        let store_id = relay.lock().unwrap().operational_state().store_id.unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let (wake, mut wakes) = mpsc::channel(1);
        let (fatal, _fatal_reports) = mpsc::channel(1);
        let handler = mj_core::runtime::spawn(serve_client_with_memory(
            server,
            relay,
            wake,
            Err("fixture has no credentials".into()),
            ConnectionRuntime::default(),
            fatal,
        ));
        let (read, mut writer) = client.into_split();
        let mut reader = BufReader::new(read);
        request(
            &mut reader,
            &mut writer,
            "hello",
            RelayRequest::Hello {
                controller_version: "test".into(),
                supported: RelayVersionRange::CURRENT,
            },
        )
        .await;
        let held = request(
            &mut reader,
            &mut writer,
            "hold",
            RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move".into(),
                expected_store_id: store_id.clone(),
            },
        )
        .await;
        assert!(matches!(
            held.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::InPlaceMoveDispatchHeld { .. }
            }
        ));
        assert_eq!(wakes.try_recv(), Err(mpsc::error::TryRecvError::Empty));

        let released = request(
            &mut reader,
            &mut writer,
            "release",
            RelayRequest::ReleaseInPlaceMoveDispatch {
                expected_harness_restarted: None,
                operation_id: "move".into(),
                expected_store_id: store_id,
            },
        )
        .await;
        assert!(matches!(
            released.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::InPlaceMoveDispatchReleased
            }
        ));
        // A continuation can already be pending when the worker releases the
        // hold. The successful reply must wake dispatch before it is written.
        assert_eq!(wakes.try_recv(), Ok(()));
        writer.shutdown().await.unwrap();
        handler.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn in_place_move_retired_logins_use_only_the_connection_and_ack_durably() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join("retired-logins")).unwrap();
        let login = mj_core::credentials::RetiredLoginSnapshot {
            id: "move".into(),
            profile_id: "source".into(),
            harness: HarnessKind::Codex,
            store_id: "store".into(),
            login_relative_path: "auth.json".into(),
            bytes: serde_json::to_vec(&serde_json::json!({"secret":"x".repeat(70000)})).unwrap(),
        };
        std::fs::write(
            root.join("retired-logins/move.json"),
            serde_json::to_vec(&serde_json::json!({
                "operation_id":"move", "store_id":"store", "login":login,
            }))
            .unwrap(),
        )
        .unwrap();
        let relay = test_relay(root);
        let before = relay.lock().unwrap().operational_state();
        let (client, server) = UnixStream::pair().unwrap();
        let (wake, _wakes) = mpsc::channel(4);
        let (fatal, _fatal_reports) = mpsc::channel(1);
        let handler = mj_core::runtime::spawn(serve_client_with_shared_credentials(
            server,
            relay.clone(),
            wake,
            SharedCredentials::new(Err("no login".into())),
            ConnectionRuntime::default(),
            fatal,
        ));
        let (read, mut writer) = client.into_split();
        let mut reader = BufReader::new(read);
        std::fs::write(
            root.join(mj_core::worker_launch::PENDING_RELOAD_FILE),
            "pending",
        )
        .unwrap();
        let deferred = request(
            &mut reader,
            &mut writer,
            "deferred-retired",
            RelayRequest::ReadRetiredLogins { after: None },
        )
        .await;
        assert!(matches!(deferred.body, RelayResponseBody::Ok {
            payload: RelayResponsePayload::RetiredLogins { deferred: true, logins, .. }
        } if logins.is_empty()));
        std::fs::remove_file(root.join(mj_core::worker_launch::PENDING_RELOAD_FILE)).unwrap();
        let response = request(
            &mut reader,
            &mut writer,
            "read-retired",
            RelayRequest::ReadRetiredLogins { after: None },
        )
        .await;
        assert!(
            matches!(response.body, RelayResponseBody::Ok { payload: RelayResponsePayload::RetiredLogins { logins, .. } } if logins == vec![login.clone()])
        );
        let response = request(
            &mut reader,
            &mut writer,
            "ack-retired",
            RelayRequest::AckRetiredLogin {
                id: "move".into(),
                fingerprint: mj_core::credentials::credential_fingerprint(&login.bytes),
            },
        )
        .await;
        assert!(matches!(response.body, RelayResponseBody::Ok { .. }));
        assert!(!root.join("retired-logins/move.json").exists());
        assert_eq!(relay.lock().unwrap().operational_state(), before);
        assert!(!(RelayRequest::ReadRetiredLogins { after: None }).supported_at(92));
        assert!(
            !RelayRequest::AckRetiredLogin {
                id: "move".into(),
                fingerprint: mj_core::credentials::credential_fingerprint(&login.bytes)
            }
            .supported_at(92)
        );
        writer.shutdown().await.unwrap();
        handler.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn in_place_move_retired_listing_serializes_with_ack_on_another_connection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("retired-logins")).unwrap();
        let login = mj_core::credentials::RetiredLoginSnapshot {
            id: "move".into(),
            profile_id: "source".into(),
            harness: HarnessKind::Codex,
            store_id: "store".into(),
            login_relative_path: "auth.json".into(),
            bytes: serde_json::to_vec(&serde_json::json!({"padding":"x".repeat(70000)})).unwrap(),
        };
        std::fs::write(
            root.path().join("retired-logins/move.json"),
            serde_json::to_vec(
                &serde_json::json!({"operation_id":"move","store_id":"store","login":login}),
            )
            .unwrap(),
        )
        .unwrap();
        let credentials = SharedCredentials::new(Err("no login".into()));
        let relay = test_relay(root.path());
        let mut connections = Vec::new();
        for _ in 0..2 {
            let (client, server) = UnixStream::pair().unwrap();
            let (wake, _wakes) = mpsc::channel(4);
            let (fatal, _fatal_reports) = mpsc::channel(1);
            let handler = mj_core::runtime::spawn(serve_client_with_shared_credentials(
                server,
                relay.clone(),
                wake,
                credentials.clone(),
                ConnectionRuntime::default(),
                fatal,
            ));
            let (read, writer) = client.into_split();
            connections.push((BufReader::new(read), writer, handler));
        }
        let (mut reader1, mut writer1, handler1) = connections.pop().unwrap();
        let (mut reader2, mut writer2, handler2) = connections.pop().unwrap();
        let lease = credentials.read().await;
        let (acked, listed) = {
            let ack = request(
                &mut reader1,
                &mut writer1,
                "ack",
                RelayRequest::AckRetiredLogin {
                    id: "move".into(),
                    fingerprint: mj_core::credentials::credential_fingerprint(&login.bytes),
                },
            );
            tokio::pin!(ack);
            tokio::time::timeout(crate::test_support::IO_TIMEOUT, async {
                loop {
                    tokio::select! {
                        _ = &mut ack => panic!("acknowledgement bypassed an active listing read guard"),
                        _ = tokio::time::sleep(Duration::from_millis(1)) => {},
                    }
                    if credentials.0.try_read().is_err() { break; }
                }
            }).await.unwrap();
            // The queued exclusive acknowledgement must finish before a new
            // listing can enumerate/open files. This makes unlink/read races
            // impossible rather than relying on repeatedly winning the race.
            let read = request(
                &mut reader2,
                &mut writer2,
                "read",
                RelayRequest::ReadRetiredLogins { after: None },
            );
            drop(lease);
            tokio::join!(&mut ack, read)
        };
        assert!(matches!(acked.body, RelayResponseBody::Ok { .. }));
        assert!(
            matches!(listed.body, RelayResponseBody::Ok { payload:RelayResponsePayload::RetiredLogins { logins, has_more:false, .. } } if logins.is_empty())
        );
        writer1.shutdown().await.unwrap();
        writer2.shutdown().await.unwrap();
        handler1.await.unwrap().unwrap();
        handler2.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn in_place_move_hold_drains_and_blocks_credential_requests_until_released() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        let launch = super::super::relay_tests::launch_config(profile.to_str().unwrap());
        let credentials = SharedCredentials::new(super::super::credential_endpoint(&launch));
        let relay = test_relay(temp.path());
        let store_id = relay.lock().unwrap().operational_state().store_id.unwrap();
        let (client, server) = UnixStream::pair().unwrap();
        let (wake, _wakes) = mpsc::channel(4);
        let (fatal, _fatal_reports) = mpsc::channel(1);
        let handler = mj_core::runtime::spawn(serve_client_with_shared_credentials(
            server,
            relay,
            wake,
            credentials.clone(),
            ConnectionRuntime::default(),
            fatal,
        ));
        let (read, mut writer) = client.into_split();
        let mut reader = BufReader::new(read);

        // Hold a read guard simulating an in-flight credential request on another
        // connection. HoldInPlaceMoveDispatch must wait for it to drain before
        // returning InPlaceMoveDispatchHeld.
        let in_flight = credentials.read().await;
        let held = {
            let hold_fut = request(
                &mut reader,
                &mut writer,
                "hold",
                RelayRequest::HoldInPlaceMoveDispatch {
                    operation_id: "move-cred".into(),
                    expected_store_id: store_id.clone(),
                },
            );
            tokio::pin!(hold_fut);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut hold_fut)
                    .await
                    .is_err(),
                "HoldInPlaceMoveDispatch must wait for in-flight credential requests to finish"
            );
            drop(in_flight);
            hold_fut.await
        };
        assert!(matches!(
            held.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::InPlaceMoveDispatchHeld { .. }
            }
        ));

        // While the dispatch hold is active, credential RPCs are refused with retryable InvalidState.
        let refused = request(
            &mut reader,
            &mut writer,
            "cred-held",
            RelayRequest::CredentialState,
        )
        .await;
        assert!(matches!(
            refused.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    retryable: true,
                    ..
                }
            }
        ));

        // Releasing the dispatch hold re-admits credential RPCs.
        let released = request(
            &mut reader,
            &mut writer,
            "release",
            RelayRequest::ReleaseInPlaceMoveDispatch {
                expected_harness_restarted: None,
                operation_id: "move-cred".into(),
                expected_store_id: store_id,
            },
        )
        .await;
        assert!(matches!(
            released.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::InPlaceMoveDispatchReleased
            }
        ));
        let allowed = request(
            &mut reader,
            &mut writer,
            "cred-released",
            RelayRequest::CredentialState,
        )
        .await;
        assert!(matches!(
            allowed.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::CredentialState { .. }
            }
        ));
        writer.shutdown().await.unwrap();
        handler.await.unwrap().unwrap();
    }

    // Hard-won: #1672: in-place Move restarted the harness without the session Git setup
    #[tokio::test]
    async fn restart_harness_in_place_preserves_session_git_environment() {
        exercise_git_setup_restart(false).await;
    }

    #[tokio::test]
    async fn rejected_opt_out_restart_preserves_live_git_setup_and_token() {
        exercise_git_setup_restart(true).await;
    }

    async fn exercise_git_setup_restart(reject_opt_out: bool) {
        let _ = mj_harness_install::login_environment::initialize_from_parent();
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let mut launch = super::super::relay_tests::launch_config(root.to_str().unwrap());
        launch.cwd = root.to_owned();
        launch.harness = HarnessKind::Claude;
        launch.environment.clear();
        launch.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            root.to_string_lossy().into_owned(),
        );
        launch
            .target_environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        launch.excluded_environment.push("MJ_TOOL_CACHE_DIR".into());
        let git_include = root.join("image-gitconfig");
        launch.target_environment.insert(
            mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH.into(),
            git_include.to_string_lossy().into_owned(),
        );
        let relay = Arc::new(RelayCell::new(
            DurableRelay::open(root, &launch.session_id, "1.0.0").unwrap(),
        ));
        let store_id = relay.lock().unwrap().operational_state().store_id.unwrap();

        let mut startup_config = launch.clone();
        initialize_launch_environment(&mut startup_config, root);
        super::super::enforce_execution_policy(&mut startup_config).unwrap();
        let (status, _) = watch::channel(PreparationSnapshot::default());
        let startup = Box::pin(prepare_harness_environment(
            root,
            &mut startup_config,
            &relay,
            Some(&status),
            &CancellationToken::new(),
            true,
        ))
        .await
        .unwrap();
        let source_config = startup.git_setup.path().join("gitconfig");
        let source_wrapper = startup.git_setup.path().join("bin/gh");
        let expected = startup.launch.spec.environment;
        let expected_path: Vec<_> = std::env::split_paths(&expected["PATH"]).collect();
        assert!(
            expected_path.contains(&startup.git_setup.path().join("bin")),
            "the accepted Git setup remains on PATH: {expected_path:?}"
        );
        assert_eq!(
            std::path::Path::new(&expected["GIT_CONFIG_GLOBAL"]),
            source_config
        );
        let source_config_before = std::fs::read_to_string(&source_config).unwrap();
        assert!(source_config_before.contains(&*git_include.to_string_lossy()));
        let source_wrapper_before = std::fs::read(&source_wrapper).unwrap();
        publish_git_setup(root, startup.git_setup).unwrap();
        let token_path = root.join("github-token");
        // The daemon may have refreshed the original container token since startup.
        mj_core::credentials::write_github_token(&token_path, b"refreshed-test-token").unwrap();
        let token_before = std::fs::read(&token_path).unwrap();

        let _ = relay.lock().unwrap().handle(RelayRequestEnvelope {
            request_id: "hold".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move".into(),
                expected_store_id: store_id.clone(),
            },
        });
        let credentials = SharedCredentials::new(super::super::credential_endpoint(&launch));
        launch.no_github_credentials = reject_opt_out;
        let (commands, mut receiver) = mpsc::channel(1);
        let reload_relay = relay.clone();
        let reload = mj_core::runtime::spawn(async move {
            let Some(CommandRequest::ReloadLaunchConfig {
                supervisor_spec,
                environment,
                on_restart,
                resolved,
                ..
            }) = receiver.recv().await
            else {
                panic!("expected a replacement harness launch");
            };
            if reject_opt_out {
                resolved
                    .send(crate::acp::ReloadResolution::RefusedBeforeCommit(
                        "turn active".into(),
                    ))
                    .unwrap();
            } else {
                apply_harness_reload_transition(
                    &reload_relay,
                    &mut TaskMonitors::default(),
                    *on_restart.unwrap(),
                )
                .await
                .unwrap();
                resolved
                    .send(crate::acp::ReloadResolution::Started)
                    .unwrap();
            }
            let (_, spec) = *supervisor_spec.unwrap();
            (spec.environment, environment)
        });
        let response = Box::pin(restart_harness_in_place_response(
            RelayRequestEnvelope {
                request_id: "reload".into(),
                protocol_version: 90,
                request: RelayRequest::RestartHarnessInPlace {
                    candidate_id: None,
                    operation_id: "move".into(),
                    expected_store_id: store_id,
                    launch_config: Box::new(launch),
                },
            },
            Some(&commands),
            &relay,
            &credentials,
            root,
            &SharedSubagents::default(),
            None,
        ))
        .await;
        drop(commands);
        let (spec_environment, shell_environment) = reload.await.unwrap();
        let candidate_config = std::path::Path::new(&spec_environment["GIT_CONFIG_GLOBAL"]);
        let candidate_dir = candidate_config.parent().unwrap();
        assert!(candidate_dir.starts_with(root));
        assert_ne!(candidate_config, source_config);
        let mut export_environment = BTreeMap::new();
        attach_session_git_environment(root, &mut export_environment, false).unwrap();
        if reject_opt_out {
            assert!(matches!(response.body, RelayResponseBody::Error { .. }));
            assert_eq!(spec_environment["MJ_GITHUB_CREDENTIALS"], "disabled");
            assert!(
                !candidate_dir.exists(),
                "rejected candidate should be removed"
            );
            assert_eq!(
                std::path::Path::new(&export_environment["GIT_CONFIG_GLOBAL"]),
                source_config
            );
        } else {
            assert!(
                matches!(
                    response.body,
                    RelayResponseBody::Ok {
                        payload: RelayResponsePayload::HarnessRestartedInPlace
                    }
                ),
                "{:?}",
                response.body
            );
            let replacement_path: Vec<_> =
                std::env::split_paths(&spec_environment["PATH"]).collect();
            let source_index = expected_path
                .iter()
                .position(|entry| entry == &source_config.parent().unwrap().join("bin"))
                .expect("the accepted Git setup is on PATH");
            assert_eq!(replacement_path[source_index], candidate_dir.join("bin"));
            assert_eq!(
                replacement_path[..source_index],
                expected_path[..source_index]
            );
            assert_eq!(
                replacement_path[source_index + 1..],
                expected_path[source_index + 1..]
            );
            for name in ["PATH", "GIT_CONFIG_GLOBAL"] {
                assert_eq!(shell_environment.get(name), spec_environment.get(name));
            }
            assert!(
                std::fs::read_to_string(candidate_config)
                    .unwrap()
                    .contains(&*git_include.to_string_lossy())
            );
            assert_eq!(
                std::path::Path::new(&export_environment["GIT_CONFIG_GLOBAL"]),
                candidate_config
            );
        }
        assert_eq!(
            std::fs::read_to_string(&source_config).unwrap(),
            source_config_before
        );
        assert_eq!(
            std::fs::read(&source_wrapper).unwrap(),
            source_wrapper_before
        );
        assert_eq!(std::fs::read(&token_path).unwrap(), token_before);
    }

    #[tokio::test]
    async fn restart_refuses_while_a_timed_out_preparation_writer_is_live() {
        let _ = mj_harness_install::login_environment::initialize_from_parent();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let relay = test_relay(root);
        let profile = root.join("profile-setting");
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let (entered, writer_started) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let first = PreparationWriter::try_acquire(
            &relay.preparation_writer,
            "in-place harness restart move-a".into(),
        )
        .unwrap();
        let first_profile = profile.clone();
        let abandoned = {
            let attempt = first.track(&cancel, async {
                let budget = PreparationStepBudget::new("profile", Duration::from_millis(200));
                bounded_blocking_preparation_step(&budget, &cancel, move |_| {
                    let setup = new_git_setup(first_profile.parent().unwrap())?;
                    let _ = entered.send(setup.path().to_owned());
                    stalled.recv().context("release first preparation writer")?;
                    std::fs::write(first_profile, "first")?;
                    Ok(())
                })
                .await
            });
            tokio::pin!(attempt);
            let abandoned = tokio::select! {
                result = &mut attempt => panic!("writer did not start: {result:?}"),
                started = writer_started => started.unwrap(),
            };
            assert!(attempt.await.unwrap_err().to_string().contains("deadline"));
            abandoned
        };
        drop(first);
        assert!(abandoned.exists(), "first writer still owns its files");

        let store_id = relay.lock().unwrap().operational_state().store_id.unwrap();
        let _ = relay.lock().unwrap().handle(RelayRequestEnvelope {
            request_id: "hold".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move-bc".into(),
                expected_store_id: store_id.clone(),
            },
        });
        let mut launch = super::super::relay_tests::launch_config(root.to_str().unwrap());
        launch.cwd = root.to_owned();
        let credentials = SharedCredentials::new(super::super::credential_endpoint(&launch));
        let (commands, mut receiver) = mpsc::channel(1);
        let request = |request_id: &str| RelayRequestEnvelope {
            request_id: request_id.into(),
            protocol_version: 90,
            request: RelayRequest::RestartHarnessInPlace {
                candidate_id: None,
                operation_id: "move-bc".into(),
                expected_store_id: store_id.clone(),
                launch_config: Box::new(launch.clone()),
            },
        };
        let refusal = tokio::time::timeout(
            Duration::from_secs(5),
            restart_harness_in_place_response(
                request("restart-b"),
                Some(&commands),
                &relay,
                &credentials,
                root,
                &SharedSubagents::default(),
                None,
            ),
        )
        .await
        .expect("restart B must return while the first writer is still blocked");
        let RelayResponseBody::Error { error } = refusal.body else {
            panic!("expected a refusal while restart A holds the gate");
        };
        assert_eq!(error.code, RelayErrorCode::InvalidState);
        assert!(!error.retryable);
        assert!(
            error
                .message
                .contains("restart move-a is already in progress at profile")
        );
        let (_, timestamp) = error.message.split_once("(started ").unwrap();
        chrono::DateTime::parse_from_rfc3339(timestamp.split_once(')').unwrap().0).unwrap();
        assert!(
            error
                .message
                .contains("Retry after it finishes, or restart the worker or session")
        );
        assert!(receiver.try_recv().is_err());
        assert!(abandoned.exists());
        assert!(!profile.exists());

        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while relay.preparation_writer.0.lock().unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finishing the first writer must release the gate");
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), "first");
        assert!(
            !abandoned.exists(),
            "first writer must retire its generation"
        );

        let reload_relay = relay.clone();
        let accepted = mj_core::runtime::spawn(async move {
            let Some(CommandRequest::ReloadLaunchConfig {
                on_restart,
                resolved,
                ..
            }) = receiver.recv().await
            else {
                panic!("expected restart C to reach the ACP runtime");
            };
            apply_harness_reload_transition(
                &reload_relay,
                &mut TaskMonitors::default(),
                *on_restart.unwrap(),
            )
            .await
            .unwrap();
            resolved
                .send(crate::acp::ReloadResolution::Started)
                .unwrap();
        });
        let response = restart_harness_in_place_response(
            request("restart-c"),
            Some(&commands),
            &relay,
            &credentials,
            root,
            &SharedSubagents::default(),
            None,
        )
        .await;
        drop(commands);
        assert!(
            matches!(
                response.body,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::HarnessRestartedInPlace
                }
            ),
            "{response:?}"
        );
        accepted.await.unwrap();
    }

    #[tokio::test]
    async fn wait_for_change_subscribes_to_lazily_started_subagents() {
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            DurableRelay::open(root.path(), "session-1", "1.0.0").unwrap(),
        ));
        let (commands, _receiver) = mpsc::channel(1);
        let (preparation_tx, preparation_rx) = watch::channel(PreparationSnapshot {
            services: Some(Arc::new(PreparedConnectionServices {
                commands,
                reviewer: Arc::new(ReviewCapture {
                    session_id: "session-1".into(),
                    cwd: root.path().into(),
                    additional_directories: vec![],
                    primary_relay: relay.clone(),
                    operation: tokio::sync::Mutex::new(()),
                }),
                subagents: None,
            })),
            ..Default::default()
        });
        let mut state = relay.observe().unwrap().operational_state();
        state.harness_preparation = None;
        let digest = mj_core::relay::relay_state_digest(&state).unwrap();
        let mut feed = super::super::subagents::SubagentFeed::default();
        // Acknowledge the durable queue's empty generation without starting
        // a listener, so publication alone does not complete this wait.
        let empty = super::super::subagents::SubagentEndpoint::open(root.path()).unwrap();
        let page = feed.poll(Some(&empty), None).unwrap();
        feed.poll(Some(&empty), Some(page.sequence)).unwrap();
        assert!(!feed.has_unpolled(&empty));
        drop(empty);
        let mut guardian_generation = 0;
        let wait = wait_for_change(
            &relay,
            Some(&preparation_rx),
            None,
            &feed,
            &mut guardian_generation,
            state.latest_ordinal,
            Some(&digest),
        );
        tokio::pin!(wait);
        tokio::select! {
            biased;
            result = &mut wait => panic!("wait returned before socket creation: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        let owner = SharedSubagents::default();
        let _shutdown = SharedSubagentsShutdownGuard(owner.clone());
        let endpoint = owner
            .ensure_serving(
                &tokio::runtime::Handle::current(),
                root.path(),
                relay.clone(),
                HarnessKind::Antigravity,
                None,
                Some(&preparation_tx),
            )
            .unwrap();
        // Consume the publication without a queued request: the existing wait
        // must subscribe to the new endpoint's generation changes.
        tokio::select! {
            biased;
            result = &mut wait => panic!("empty socket publication changed state: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        endpoint
            .enqueue(mj_core::subagent::SubagentToolRequest {
                originating_command_id: None,
                request_id: "lazy-queue".into(),
                created_at_ms: 1,
                action: mj_core::subagent::SubagentToolAction::ListAgents,
            })
            .unwrap();
        let observed = tokio::time::timeout(std::time::Duration::from_secs(2), wait)
            .await
            .expect("new endpoint queue must wake the active wait promptly")
            .unwrap();
        assert!(
            matches!(
                observed,
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::ChangeObserved {
                        subagent_queue_changed: true,
                        ..
                    }
                }
            ),
            "{observed:?}"
        );
    }

    // Hard-won: d796c7d91: in-place reload discarded the new MCP registration mode.
    #[tokio::test]
    async fn restart_harness_in_place_lazily_starts_subagent_socket() {
        let _ = mj_harness_install::login_environment::initialize_from_parent();
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            DurableRelay::open(root.path(), "session-1", "1.0.0").unwrap(),
        ));
        let store_id = relay
            .lock()
            .unwrap()
            .operational_state()
            .store_id
            .clone()
            .unwrap();
        let mut config = super::super::relay_tests::launch_config(root.path().to_str().unwrap());
        config.cwd = root.path().to_path_buf();
        config.harness = HarnessKind::Antigravity;
        config.subagents = mj_core::subagent::SubagentPolicy::Mjolnir {
            straightforward: None,
            subtle: None,
            orchestrators: false,
        };
        config.review_trigger = mj_core::config::ReviewTrigger::Push;
        let home = root.path().join("agy-home");
        config
            .environment
            .insert("MJ_ANTIGRAVITY_HOME".into(), home.to_str().unwrap().into());
        config
            .environment
            .insert("MJ_SESSION_MESSAGE_MCP".into(), "1".into());
        let credentials = SharedCredentials::new(super::super::credential_endpoint(&config));
        let request = || RelayRequestEnvelope {
            request_id: "restart".into(),
            protocol_version: 90,
            request: RelayRequest::RestartHarnessInPlace {
                candidate_id: None,
                operation_id: "move".into(),
                expected_store_id: store_id.clone(),
                launch_config: Box::new(config.clone()),
            },
        };
        let _ = relay.lock().unwrap().handle(RelayRequestEnvelope {
            request_id: "hold".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move".into(),
                expected_store_id: store_id.clone(),
            },
        });
        let shared_subagents = SharedSubagents::default();
        let _shutdown = SharedSubagentsShutdownGuard(shared_subagents.clone());
        let (commands, mut receiver) = mpsc::channel(1);
        let (preparation_tx, preparation_rx) = watch::channel(PreparationSnapshot {
            state: Some(mj_core::relay::HarnessPreparation::Started),
            services: Some(Arc::new(PreparedConnectionServices {
                commands: commands.clone(),
                reviewer: Arc::new(ReviewCapture {
                    session_id: "session-1".into(),
                    cwd: root.path().into(),
                    additional_directories: vec![],
                    primary_relay: relay.clone(),
                    operation: tokio::sync::Mutex::new(()),
                }),
                subagents: None,
            })),
            ..Default::default()
        });
        // Private bridge resolution must fail before the lazy listener is bound.
        std::fs::write(root.path().join(".claude.json"), b"not json").unwrap();
        let mut malformed = request();
        if let RelayRequest::RestartHarnessInPlace { launch_config, .. } = &mut malformed.request {
            launch_config.harness = HarnessKind::Claude;
            launch_config.bridge_command = root.path().join("missing-reload-bridge");
        }
        let response = restart_harness_in_place_response(
            malformed,
            Some(&commands),
            &relay,
            &credentials,
            root.path(),
            &shared_subagents,
            Some(&preparation_tx),
        )
        .await;
        let RelayResponseBody::Error { error } = response.body else {
            panic!("expected malformed profile failure")
        };
        assert_eq!(error.code, RelayErrorCode::InvalidState);
        assert!(
            error.message.contains("prepare updated harness launch"),
            "{error:?}"
        );
        let socket_path = root.path().join(super::super::subagents::SUBAGENT_SOCKET);
        assert!(!socket_path.exists());
        assert!(shared_subagents.endpoint().is_none());
        assert!(receiver.try_recv().is_err());
        std::fs::write(root.path().join(".claude.json"), b"{}").unwrap();
        let closed = SharedSubagents::default();
        closed.stop();
        let refused = restart_harness_in_place_response(
            request(),
            Some(&commands),
            &relay,
            &credentials,
            root.path(),
            &closed,
            Some(&preparation_tx),
        )
        .await;
        let RelayResponseBody::Error { error } = refused.body else {
            panic!("expected closed owner refusal")
        };
        assert_eq!(error.code, RelayErrorCode::InvalidState);
        assert!(error.message.contains("worker is closing"), "{error:?}");
        assert!(receiver.try_recv().is_err());
        assert!(!socket_path.exists());
        let acceptor = mj_core::runtime::spawn(async move {
            let Some(CommandRequest::ReloadLaunchConfig {
                subagent_mcp_socket,
                managed_subagent,
                resolved,
                ..
            }) = receiver.recv().await
            else {
                panic!("expected reload")
            };
            let socket = subagent_mcp_socket.unwrap();
            assert!(!socket.profile_registration);
            assert_eq!(socket.role, mj_core::subagent::SubagentMcpRole::Parent);
            assert!(socket.review_tools);
            assert!(!managed_subagent);
            resolved
                .send(crate::acp::ReloadResolution::Started)
                .unwrap();
        });
        let response = restart_harness_in_place_response(
            request(),
            Some(&commands),
            &relay,
            &credentials,
            root.path(),
            &shared_subagents,
            Some(&preparation_tx),
        )
        .await;
        assert!(
            matches!(response.body, RelayResponseBody::Ok { .. }),
            "{response:?}"
        );
        acceptor.await.unwrap();
        assert!(socket_path.exists());
        assert!(shared_subagents.endpoint().is_some());
        assert!(
            preparation_rx
                .borrow()
                .services
                .as_ref()
                .unwrap()
                .subagents
                .is_some()
        );
        assert!(preparation_rx.has_changed().unwrap());
        shared_subagents.stop();
        assert!(!socket_path.exists());
    }

    // Hard-won: e2c147489: an in-place profile switch discarded Claude's agent and review tools.
    #[tokio::test]
    async fn restart_harness_in_place_does_not_overwrite_supervisor_spec_when_rejected() {
        let _ = mj_harness_install::login_environment::initialize_from_parent();
        let root = tempfile::tempdir().unwrap();
        let supervisor_path = root.path().join("acp-supervisor.json");
        let launch_path = root.path().join("launch.json");
        std::fs::write(&supervisor_path, "original-supervisor-spec").unwrap();
        std::fs::write(&launch_path, "original-launch-json").unwrap();

        let relay = Arc::new(RelayCell::new(
            DurableRelay::open(root.path(), "session-1", "1.0.0").unwrap(),
        ));
        let store_id = relay
            .lock()
            .unwrap()
            .operational_state()
            .store_id
            .clone()
            .expect("store_id");

        // No hold active -> rejected without touching acp-supervisor.json or credentials.
        let launch_config = super::super::relay_tests::launch_config(root.path().to_str().unwrap());
        let initial_credentials = super::super::credential_endpoint(&launch_config);
        let shared_credentials = SharedCredentials::new(initial_credentials.clone());
        {
            let mut state = relay.lock().unwrap();
            state.set_background_work_policy(crate::relay::BackgroundWorkPolicy::AntigravityTasks);
            state.retire_harness_process_state().unwrap();
            assert_eq!(state.operational_state().background_work_known, Some(false));
        }
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let response = restart_harness_in_place_response(
            RelayRequestEnvelope {
                request_id: "r-restart-unheld".into(),
                protocol_version: 90,
                request: RelayRequest::RestartHarnessInPlace {
                    candidate_id: None,
                    operation_id: "move-op-1".into(),
                    expected_store_id: store_id.clone(),
                    launch_config: Box::new(launch_config.clone()),
                },
            },
            Some(&commands_tx),
            &relay,
            &shared_credentials,
            root.path(),
            &SharedSubagents::default(),
            None,
        )
        .await;
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: mj_core::relay::RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));
        assert_eq!(
            std::fs::read_to_string(&supervisor_path).unwrap(),
            "original-supervisor-spec"
        );
        assert_eq!(shared_credentials.snapshot().await, initial_credentials);
        assert!(commands_rx.try_recv().is_err());

        // Hold active, but ACP rejects the reload -> still does not overwrite acp-supervisor.json.
        let _ = relay.lock().unwrap().handle(RelayRequestEnvelope {
            request_id: "r-hold".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move-op-1".into(),
                expected_store_id: store_id.clone(),
            },
        });
        let rejector = mj_core::runtime::spawn(async move {
            if let Some(CommandRequest::ReloadLaunchConfig { resolved, .. }) =
                commands_rx.recv().await
            {
                let _ = resolved.send(crate::acp::ReloadResolution::RefusedBeforeCommit(
                    "turn active".into(),
                ));
            }
        });
        let response = restart_harness_in_place_response(
            RelayRequestEnvelope {
                request_id: "r-restart-rejected".into(),
                protocol_version: 90,
                request: RelayRequest::RestartHarnessInPlace {
                    candidate_id: None,
                    operation_id: "move-op-1".into(),
                    expected_store_id: store_id.clone(),
                    launch_config: Box::new(launch_config),
                },
            },
            Some(&commands_tx),
            &relay,
            &shared_credentials,
            root.path(),
            &SharedSubagents::default(),
            None,
        )
        .await;
        drop(commands_tx);
        rejector.await.unwrap();
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: mj_core::relay::RelayProtocolError {
                    code: RelayErrorCode::InvalidState,
                    ..
                }
            }
        ));
        assert_eq!(
            std::fs::read_to_string(&supervisor_path).unwrap(),
            "original-supervisor-spec"
        );
        assert_eq!(
            std::fs::read_to_string(&launch_path).unwrap(),
            "original-launch-json"
        );
        assert_eq!(shared_credentials.snapshot().await, initial_credentials);
        // When ACP accepts the reload, launch.json IS updated, harness_restarted is marked,
        // shared credentials switch to the restarted harness/home, and the reloaded environment
        // merges target_environment + MJ_KEEP_DIR while excluding disallowed keys and preserving
        // the raw launch.json payload.
        let (ok_tx, mut ok_rx) = mpsc::channel(4);
        let dest_home = root.path().join("profile");
        std::fs::create_dir_all(&dest_home).unwrap();
        let profile_to_read = dest_home.clone();
        let worker_root = root.path().to_owned();
        let reload_relay = relay.clone();
        let acceptor = mj_core::runtime::spawn(async move {
            match ok_rx.recv().await {
                Some(CommandRequest::ReloadLaunchConfig {
                    environment,
                    supervisor_spec,
                    on_restart,
                    resolved,
                    ..
                }) => {
                    apply_harness_reload_transition(
                        &reload_relay,
                        &mut TaskMonitors::default(),
                        *on_restart.unwrap(),
                    )
                    .await
                    .unwrap();
                    // Read what the replacement Claude process will read,
                    // before acknowledging its startup.
                    let config: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(profile_to_read.join(".claude.json")).unwrap(),
                    )
                    .unwrap();
                    for server in ["mj-agents", "mj-review"] {
                        assert_eq!(
                            config["mcpServers"][server]["command"],
                            serde_json::json!(worker_root.join("hel"))
                        );
                    }
                    assert_eq!(config["mcpServers"]["mj-agents"]["alwaysLoad"], true);
                    let settings: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(profile_to_read.join("settings.json")).unwrap(),
                    )
                    .unwrap();
                    let allow = settings["permissions"]["allow"].as_array().unwrap();
                    assert!(allow.contains(&serde_json::json!("Bash(rg)")));
                    for tool in [
                        "mcp__mj-agents__send_message",
                        "mcp__mj-agents__spawn_worker",
                        "mcp__mj-review__request_review",
                    ] {
                        assert!(allow.contains(&serde_json::json!(tool)), "missing {tool}");
                    }
                    let _ = resolved.send(crate::acp::ReloadResolution::Started);
                    Some((environment, supervisor_spec))
                }
                _ => None,
            }
        });
        let dest_cred_bytes = br#"{"claudeAiOauth":{"accessToken":"reloaded-tok"}}"#;
        let dest_marker =
            mj_core::config::harness_authentication_marker(HarnessKind::Claude, &dest_home);
        std::fs::write(&dest_marker, dest_cred_bytes).unwrap();
        let mut updated_launch =
            super::super::relay_tests::launch_config(dest_home.to_str().unwrap());
        updated_launch.harness = HarnessKind::Claude;
        updated_launch.subagents = serde_json::from_str(r#"{"mode":"mjolnir"}"#).unwrap();
        updated_launch.review_trigger = mj_core::config::ReviewTrigger::Push;
        updated_launch.harness_home = dest_home.clone();
        updated_launch.environment.clear();
        updated_launch.environment.insert(
            "CLAUDE_CONFIG_DIR".into(),
            dest_home.to_string_lossy().into_owned(),
        );
        updated_launch.session_id = "session-1".into();
        updated_launch.cwd = root.path().to_path_buf();
        updated_launch.initial_model = Some("reloaded-model".into());
        updated_launch
            .target_environment
            .insert("MJ_TARGET_MARKER".into(), "from-target".into());
        updated_launch.target_environment.insert(
            mj_core::jev::FASTINO_KEY_ENVIRONMENT.into(),
            "secret-fastino".into(),
        );
        updated_launch
            .excluded_environment
            .push("EXCLUDED_SECRET".into());
        updated_launch
            .environment
            .insert("EXCLUDED_SECRET".into(), "must-strip".into());
        // Profile configurators now run after bridge retirement. Malformed
        // destination configuration is covered by staged post-commit failure.
        std::fs::write(dest_home.join(".claude.json"), b"{}").unwrap();
        std::fs::write(
            dest_home.join("settings.json"),
            br#"{"permissions":{"allow":["Bash(rg)"]}}"#,
        )
        .unwrap();
        relay.lock().unwrap().set_steering_supported(Some(true));
        let ok_response = restart_harness_in_place_response(
            RelayRequestEnvelope {
                request_id: "r-restart-ok".into(),
                protocol_version: 90,
                request: RelayRequest::RestartHarnessInPlace {
                    candidate_id: None,
                    operation_id: "move-op-1".into(),
                    expected_store_id: store_id,
                    launch_config: Box::new(updated_launch),
                },
            },
            Some(&ok_tx),
            &relay,
            &shared_credentials,
            root.path(),
            &SharedSubagents::default(),
            None,
        )
        .await;
        drop(ok_tx);
        let (reloaded_env, reloaded_supervisor) = acceptor
            .await
            .unwrap()
            .expect("ReloadLaunchConfig delivered");
        assert!(matches!(
            ok_response.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::HarnessRestartedInPlace
            }
        ));
        assert_eq!(
            relay
                .lock()
                .unwrap()
                .operational_state()
                .background_work_known,
            None
        );
        let refreshed_endpoint = shared_credentials
            .snapshot()
            .await
            .expect("reloaded credential endpoint");
        assert_eq!(refreshed_endpoint.harness, HarnessKind::Claude);
        assert_eq!(refreshed_endpoint.home, dest_home);
        assert_eq!(refreshed_endpoint.marker, dest_marker);
        let login_identity = relay.observe().unwrap().login_identity.clone();
        let cred_state = credential_response(
            RelayRequestEnvelope {
                request_id: "r-cred-state".into(),
                protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
                request: RelayRequest::CredentialState,
            },
            &shared_credentials.snapshot().await,
            root.path(),
            &login_identity,
        )
        .await;
        assert!(matches!(
            cred_state.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::CredentialState { present: true, .. }
            }
        ));
        assert_eq!(
            relay.lock().unwrap().authentication_login_path.as_deref(),
            Some(dest_marker.as_path())
        );
        assert_eq!(
            relay.lock().unwrap().operational_state().steering_supported,
            Some(true),
            "in-place restart into a private staged Claude home must preserve hook-backed steering"
        );
        assert_eq!(
            reloaded_env.get("MJ_TARGET_MARKER").map(String::as_str),
            Some("from-target")
        );
        assert!(
            !reloaded_env.contains_key(mj_core::jev::FASTINO_KEY_ENVIRONMENT),
            "FASTINO_KEY must be stripped from reloaded environment"
        );
        let expected_keep = mj_core::session_keep::snapshot_keep_dir(
            &std::fs::canonicalize(root.path()).unwrap(),
            Some("session-1"),
        )
        .unwrap()
        .to_string_lossy()
        .into_owned();
        assert_eq!(
            reloaded_env
                .get(mj_core::worker_launch::SESSION_KEEP_DIR_ENV)
                .map(String::as_str),
            Some(expected_keep.as_str())
        );
        let (_, spec) = *reloaded_supervisor.expect("supervisor spec sent");
        assert_eq!(
            spec.environment.get("MJ_TARGET_MARKER").map(String::as_str),
            Some("from-target")
        );
        assert_eq!(
            spec.environment
                .get(mj_core::worker_launch::SESSION_KEEP_DIR_ENV)
                .map(String::as_str),
            Some(expected_keep.as_str())
        );
        assert!(
            !spec.environment.contains_key("EXCLUDED_SECRET"),
            "excluded environment variable must be stripped from supervisor spec"
        );
        let persisted: mj_core::worker_launch::WorkerLaunchConfig =
            serde_json::from_str(&std::fs::read_to_string(&launch_path).unwrap()).unwrap();
        assert_eq!(persisted.initial_model.as_deref(), Some("reloaded-model"));
        assert!(
            !persisted.environment.contains_key("MJ_TARGET_MARKER"),
            "persisted launch.json must keep target_environment separate from environment"
        );
    }

    #[test]
    fn in_place_harness_restart_guard_clears_reservation_when_complete_fails() {
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(RelayCell::new(
            DurableRelay::open(root.path(), "session-1", "1.0.0").unwrap(),
        ));
        let store_id = relay
            .lock()
            .unwrap()
            .operational_state()
            .store_id
            .clone()
            .expect("store_id");
        let _ = relay.lock().unwrap().handle(RelayRequestEnvelope {
            request_id: "r-hold".into(),
            protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
            request: RelayRequest::HoldInPlaceMoveDispatch {
                operation_id: "move-op-1".into(),
                expected_store_id: store_id.clone(),
            },
        });
        assert_eq!(
            relay
                .lock()
                .unwrap()
                .admit_in_place_harness_restart("move-op-1", &store_id)
                .unwrap(),
            crate::relay::InPlaceHarnessRestartDecision::Proceed
        );
        let guard = InPlaceHarnessRestartGuard {
            relay: relay.clone(),
            operation_id: "move-op-1".into(),
            expected_store_id: "mismatched-store-id".into(),
            completed: false,
        };
        assert!(guard.complete().is_err());
        // Drop must have cleared in_place_harness_restart_in_flight so a retry can proceed.
        assert_eq!(
            relay
                .lock()
                .unwrap()
                .admit_in_place_harness_restart("move-op-1", &store_id)
                .unwrap(),
            crate::relay::InPlaceHarnessRestartDecision::Proceed
        );
    }
}
