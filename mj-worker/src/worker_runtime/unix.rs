mod dispatch;
mod git_env;
mod kimi;
mod requests;
mod supervisor;
pub(crate) use dispatch::*;
pub use git_env::*;
pub(crate) use kimi::*;
pub(crate) use requests::*;
pub use supervisor::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionUpdate;
use anyhow::{Context, Result, bail};
use mj_core::local_sockets::{bind_unix_listener, connect_unix_stream};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::reviewer::{ReviewerCancellation, ReviewerPlacement, ReviewerSidecar};
use super::{
    AcpSupervisorSpec, CredentialEndpoint, REVIEW_UNTRACKED_FILE, WorkerLaunchConfig,
    acp_additional_directories, reviewer_workspace_directories,
};

use crate::acp::{self, CommandRequest, LaunchSpec, RuntimeEvent};
use crate::relay::{
    ClaimedRelayCommand, DeferredRelayAttach, DurableRelay, RELAY_STATE_FILE,
    RESTORED_RELAY_SEED_FILE, RelayCommand, RelayCommandOutcome, RelayErrorCode, RelayObservation,
    RelayProtocolError, RelayRequest, RelayRequestEnvelope, RelayResponseBody,
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
const PREPARATION_CANCELLED_ERROR: &str =
    "cancelled because the session is closing or the worker is shutting down";

#[derive(Clone, Default)]
pub(super) struct PreparationSnapshot {
    state: Option<mj_core::relay::HarnessPreparation>,
    services: Option<Arc<PreparedConnectionServices>>,
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
    reviewer: Arc<ReviewerSidecar>,
    subagents: Option<super::subagents::SubagentEndpoint>,
}

struct RunningHarness {
    acp_task: tokio::task::JoinHandle<Result<()>>,
    event_task: tokio::task::JoinHandle<Result<()>>,
    acp_shutdown: CancellationToken,
    commands: mpsc::Sender<CommandRequest>,
    reviewer: Arc<ReviewerSidecar>,
    subagent_socket_guard: Option<super::subagents::SubagentSocketGuard>,
    subagents: Option<super::subagents::SubagentEndpoint>,
    harness_gc: Option<tokio::task::JoinHandle<()>>,
    shell_cleanup: tokio_util::task::TaskTracker,
}

struct HarnessPreparationCompletion {
    result: Result<RunningHarness>,
    idle_dispatch_wakes: Option<mpsc::Receiver<()>>,
}

type UntrackedReviewEntries =
    Arc<Mutex<std::collections::BTreeMap<PathBuf, Vec<crate::review::capture::UntrackedEntry>>>>;

struct HarnessPreparationSetup {
    root: PathBuf,
    config: WorkerLaunchConfig,
    relay: Arc<Mutex<DurableRelay>>,
    session_git_config_include: Option<PathBuf>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    kimi_task_home: Option<std::result::Result<PathBuf, String>>,
    relay_state_exists: bool,
    untracked_at_start: UntrackedReviewEntries,
    resume_session: Option<String>,
    native_session_may_have_history: bool,
}

#[derive(Clone)]
struct PreparationStepBudget {
    step: String,
    deadline: tokio::time::Instant,
    timeout: std::time::Duration,
}

impl PreparationStepBudget {
    fn new(step: &str, timeout: std::time::Duration) -> Self {
        Self {
            step: step.to_owned(),
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
    root: PathBuf,
    config: WorkerLaunchConfig,
    session_environment: BTreeMap<String, String>,
    supervisor_spec: AcpSupervisorSpec,
    relay: Arc<Mutex<DurableRelay>>,
    untracked_at_start: UntrackedReviewEntries,
    resume_session: Option<String>,
    native_session_may_have_history: bool,
    profile_registration: bool,
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
    reviewer: Arc<ReviewerSidecar>,
    subagents: Option<super::subagents::SubagentEndpoint>,
    subagent_socket_guard: Option<super::subagents::SubagentSocketGuard>,
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
    reviewer: Option<Arc<ReviewerSidecar>>,
    subagent_socket_guard: Option<super::subagents::SubagentSocketGuard>,
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
            reviewer: self.reviewer.take().expect("reviewer was prepared"),
            subagent_socket_guard: self.subagent_socket_guard.take(),
            subagents: self.subagents.take(),
            shell_cleanup: self
                .shell_cleanup
                .take()
                .expect("shell tracker was prepared"),
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
        root,
        config,
        session_environment,
        supervisor_spec,
        relay,
        untracked_at_start,
        resume_session,
        native_session_may_have_history,
        profile_registration,
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
    let worker_executable = std::env::current_exe().context("locate Hel worker executable")?;
    let mut reviewer_target_environment = config.target_environment.clone();
    reviewer_target_environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    let reviewer = Arc::new(ReviewerSidecar::new(
        ReviewerPlacement {
            target_environment: reviewer_target_environment,
            worker_root: root.clone(),
            session_id: config.session_id.clone(),
            cwd: config.cwd.clone(),
            additional_directories: reviewer_workspace_directories(&config),
            worker_executable: worker_executable.clone(),
            harness_runtime: config.harness_runtime,
            review_capture: config.review_capture,
            untracked_at_start,
        },
        relay.clone(),
    ));
    let (subagents, subagent_socket_guard) = if subagent_role.is_some() {
        let (endpoint, guard) =
            super::subagents::serve(&runtime, &root, relay.clone(), config.harness)?;
        (Some(endpoint), Some(guard))
    } else {
        (None, None)
    };

    let (acp_activity, step_clock, tools_in_flight, turn_context, accepted_config) = {
        let relay = relay.lock().expect("relay lock poisoned");
        (
            relay.acp_activity_clock(),
            relay.step_clock(),
            relay.tools_in_flight(),
            relay.turn_context(),
            Arc::new(Mutex::new(accepted_config)),
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
    let additional_directories = acp_additional_directories(&config);
    let acp_spec = LaunchSpec {
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
        cwd: config.cwd,
        additional_directories,
        extra_mcp_servers: Vec::new(),
        subagent_policy: if config.handback_tool {
            mj_core::subagent::SubagentPolicy::None
        } else {
            config.subagents.clone()
        },
        subagent_mcp_socket: subagent_role.map(|role| crate::acp::SubagentMcpSocket {
            path: root.join(super::subagents::SUBAGENT_SOCKET),
            role,
            agent_mailboxes_enabled: config.agent_mailboxes_enabled,
            profile_registration,
        }),
        project_memory: config.project_memory,
        resume_session,
        native_session_may_have_history,
        accepted_config,
        initial_model: config.initial_model,
        harness: config.harness,
        execution_policy: config.execution_policy,
        acp_activity,
        step_clock,
        tools_in_flight,
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

fn start_bridge(
    setup: PreparedAcpSetup,
    relay: Arc<Mutex<DurableRelay>>,
    dispatch_wakes: mpsc::Receiver<()>,
    kimi_task_home: Option<std::result::Result<PathBuf, String>>,
    runtime: tokio::runtime::Handle,
    cancel: CancellationToken,
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
    started.acp_task = Some(runtime.spawn(acp::run_with_shutdown(
        acp_spec,
        commands_rx,
        events_tx,
        acp_shutdown,
    )));
    if cancel.is_cancelled() {
        bail!("preparation cancelled while starting the ACP bridge");
    }
    started.event_task = Some(runtime.spawn(run_relay_coordinator_with_shells(
        relay,
        events_rx,
        dispatch_wakes,
        commands_tx,
        user_shells,
        kimi_task_home.map(KimiTaskMonitor::new),
    )));
    if cancel.is_cancelled() {
        bail!("preparation cancelled while starting the relay coordinator");
    }
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
    status: &watch::Sender<PreparationSnapshot>,
    step: &str,
    timeout: std::time::Duration,
    cancel: &CancellationToken,
) -> Result<PreparationStepBudget> {
    let budget = PreparationStepBudget::new(step, timeout);
    status.send_modify(|snapshot| {
        snapshot.state = Some(mj_core::relay::HarnessPreparation::Preparing {
            step: step.to_owned(),
            since_ms: chrono::Utc::now().timestamp_millis(),
        });
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
    if !snapshot.is_pending() {
        return None;
    }
    match snapshot.state.as_ref() {
        Some(mj_core::relay::HarnessPreparation::Preparing { step, .. }) => Some(format!(
            "harness is preparing at {step}; {operation} is not available yet"
        )),
        Some(mj_core::relay::HarnessPreparation::Failed { step, error, .. }) => {
            Some(format!("harness preparation failed at {step}: {error}"))
        }
        Some(mj_core::relay::HarnessPreparation::Started) => None,
        // Standalone/reviewer socket handlers have no primary preparation
        // supervisor; their explicit services are already available.
        None => None,
    }
}

fn overlay_preparation_state(response: &mut RelayResponseEnvelope, snapshot: &PreparationSnapshot) {
    let state = match &mut response.body {
        RelayResponseBody::Ok {
            payload: RelayResponsePayload::Status(state),
        }
        | RelayResponseBody::Ok {
            payload: RelayResponsePayload::Attached { state, .. },
        } => state,
        _ => return,
    };
    state.harness_preparation = snapshot.state.clone();
}

fn unavailable_request(envelope: RelayRequestEnvelope, message: String) -> RelayResponseEnvelope {
    RelayResponseEnvelope {
        request_id: envelope.request_id,
        protocol_version: envelope.protocol_version,
        body: compaction_error(RelayErrorCode::InvalidState, &message),
    }
}

#[derive(Clone)]
pub(super) struct ProjectMemoryEndpoint {
    config: Option<super::ProjectMemoryLaunchConfig>,
    io: Arc<tokio::sync::Semaphore>,
    history: super::history::HistoryEndpoint,
    cwd: PathBuf,
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use mj_core::history::{HistoryQuery, HistoryResult};
    use mj_core::relay::RELAY_PROTOCOL_VERSION;

    #[tokio::test]
    async fn history_round_trips_large_replies_without_delegation_or_durable_events() {
        let temp = tempfile::tempdir().unwrap();
        let relay = Arc::new(Mutex::new(
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
            tasks.push(tokio::spawn(serve_client_with_memory(
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
                query: HistoryQuery::TraceFile {
                    path: "a.rs".into(),
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
}

impl ProjectMemoryEndpoint {
    fn new(config: Option<super::ProjectMemoryLaunchConfig>) -> Self {
        Self {
            config,
            io: Arc::new(tokio::sync::Semaphore::new(1)),
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
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bind `control.sock`, restrict it to the owner, and record the `bind-socket`
/// and `serving` startup steps. The caller must accept on the listener
/// promptly: the daemon reads the socket's existence as "this worker answers".
fn publish_control_socket(
    root: &std::path::Path,
    socket: &std::path::Path,
) -> Result<(UnixListener, SocketGuard)> {
    super::record_startup_step(root, "bind-socket");
    let listener = bind_unix_listener(socket)
        .with_context(|| format!("bind worker socket {}", socket.display()))?;
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
    let session_git_config_include = config
        .target_environment
        .get(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH)
        .map(PathBuf::from);
    let mut target_environment = config.target_environment.clone();
    target_environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    target_environment.extend(config.environment);
    target_environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    config.environment = target_environment;
    let startup_directory = std::env::current_dir()?;
    let root = super::resolve_relative_worker_root(root, &startup_directory);
    super::resolve_relative_harness_home(&mut config, &startup_directory);
    let checkpoint_only = config.run_mode == mj_core::worker_launch::WorkerRunMode::CheckpointOnly;
    super::record_startup_step(&root, "policy");
    if !checkpoint_only {
        super::enforce_execution_policy(&mut config)?;
    }
    // Resolve this before the launch config's environment is consumed by
    // the ACP supervisor specification below.
    let credentials = super::credential_endpoint(&config);
    let kimi_task_home = (config.harness == HarnessKind::Kimi).then(|| {
        credentials
            .as_ref()
            .map(|endpoint| endpoint.home.clone())
            .map_err(Clone::clone)
    });
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create worker root {}", root.display()))?;
    let socket = root.join("control.sock");
    // Refuse a second daemon before touching durable state: opening the
    // relay recovers the journal in place, so getting that far would
    // corrupt the files a live worker is still writing.
    if socket.exists() && connect_unix_stream(&socket).is_ok() {
        bail!("a worker is already running at {}", socket.display());
    }
    // A dead daemon can leave its socket inode behind. Remove it before
    // journal recovery so controller liveness can distinguish a worker
    // that is still starting from one that has published its endpoint.
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("remove stale socket {}", socket.display()))?;
    }
    let relay_state_exists = root.join(RELAY_STATE_FILE).exists();
    let restarting = relay_state_exists || root.join(RESTORED_RELAY_SEED_FILE).exists();
    let untracked_at_start = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    // Validate and recover durable state before publishing a socket. A
    // failed startup must never leave a fresh endpoint that looks live.
    super::record_startup_step(&root, "durable-relay");
    let mut durable_relay = if checkpoint_only {
        DurableRelay::open_for_checkpoint(&root, &config.session_id, env!("CARGO_PKG_VERSION"))?
    } else {
        DurableRelay::open(&root, &config.session_id, env!("CARGO_PKG_VERSION"))?
    };
    // Hashed once, at startup: the controller compares this against the binary
    // it would install to decide whether this worker is the current build.
    durable_relay.set_worker_build(mj_core::worker_launch::running_executable_digest());
    // Only Claude Code's adapter marks the end of a turn it started on its
    // own, so only it can model those turns without leaving a session stuck
    // Running. See `.agents/docs/claude-autonomous-turns.md`.
    durable_relay.set_turn_verdict_harness(config.harness);
    durable_relay.set_continuation_enabled(!mj_core::jev::continuation_disabled_by_environment());
    durable_relay.set_harness_turn_policy(match config.harness {
        HarnessKind::Claude => crate::relay::HarnessTurnPolicy::ClaudeAdapter,
        HarnessKind::Codex => crate::relay::HarnessTurnPolicy::CodexAdapter,
        _ => crate::relay::HarnessTurnPolicy::Disabled,
    });
    // Codex runs its own shells instead of asking Hel for a terminal, so the
    // only evidence of a command it left running is a card with no exit code.
    durable_relay.set_background_work_policy(match config.harness {
        HarnessKind::Codex => crate::relay::BackgroundWorkPolicy::CodexExecCards,
        HarnessKind::Claude => crate::relay::BackgroundWorkPolicy::ClaudeTasks,
        HarnessKind::Kimi => crate::relay::BackgroundWorkPolicy::KimiTasks,
        _ => crate::relay::BackgroundWorkPolicy::HostedTerminals,
    });
    record_imported_native_identity(&config, &mut durable_relay)?;
    let resume_session = select_resume_session(&config, &durable_relay);
    let native_session_may_have_history = durable_relay.native_session_may_have_history();
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
    crate::exit_record::reserve(&root)?;

    if restarting
        && durable_relay.operational_state().execution
            != mj_core::relay::RelayExecutionState::Closed
    {
        durable_relay.record_observation(RelayObservation::SessionRestarted)?;
    }

    let relay = Arc::new(Mutex::new(durable_relay));
    // Client tasks are detached, so a durable failure they cannot recover
    // from has to travel back here to stop the daemon.
    let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
    let (cpu_tx, cpu_rx) = tokio::sync::watch::channel(Ok(None));
    let cpu_task = tokio::spawn(crate::cpu_usage::sample_cpu(cpu_tx));
    let cpu_abort = cpu_task.abort_handle();
    struct StopCpu(tokio::task::AbortHandle);
    impl Drop for StopCpu {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop_cpu = StopCpu(cpu_abort);
    let cpu_fatal = fatal_tx.clone();
    let cpu_session_id = config.session_id.clone();
    tokio::spawn(async move {
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

    let initial_step = if config.review_capture {
        "review-baseline"
    } else {
        "login-resolve"
    };
    let (preparation_tx, preparation_rx) = watch::channel(PreparationSnapshot {
        state: Some(mj_core::relay::HarnessPreparation::Preparing {
            step: initial_step.to_owned(),
            since_ms: chrono::Utc::now().timestamp_millis(),
        }),
        services: None,
    });
    let preparation_cancel = CancellationToken::new();
    let (dispatch_wake_tx, dispatch_wake_rx) = mpsc::channel(1);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install worker shutdown signal handler")?;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // The relay is recovered; accept immediately while harness work runs
    // under its own supervisor.
    let (listener, socket_guard) = publish_control_socket(&root, &socket)?;
    let subagent_role = config.subagent_mcp_role();
    let has_subagent_tools = subagent_role.is_some();
    let mut client_task = tokio::spawn(accept_worker_clients(
        listener,
        socket_guard,
        relay.clone(),
        dispatch_wake_tx.clone(),
        credentials.clone(),
        ConnectionRuntime {
            project_memory,
            cpu: Some(cpu_rx),
            preparation: Some(preparation_rx.clone()),
            preparation_cancel: Some(preparation_cancel.clone()),
            has_subagent_tools,
            ..Default::default()
        },
        fatal_tx.clone(),
    ));
    let _abort_clients_on_drop = AbortTaskOnDrop(client_task.abort_handle());

    let preparation_setup = HarnessPreparationSetup {
        root: root.clone(),
        config,
        relay: relay.clone(),
        session_git_config_include,
        credentials,
        kimi_task_home,
        relay_state_exists,
        untracked_at_start,
        resume_session,
        native_session_may_have_history,
    };
    let prep_status = preparation_tx.clone();
    let prep_cancel = preparation_cancel.clone();
    let mut prep_task = tokio::spawn(async move {
        supervise_harness_preparation(
            preparation_setup,
            dispatch_wake_rx,
            prep_status,
            prep_cancel,
        )
        .await
    });
    let _cancel_preparation_on_drop = CancelPreparationOnDrop(preparation_cancel.clone());

    let preparation_result = tokio::select! {
        biased;
        result = &mut prep_task => Some(result),
        _ = preparation_cancel.cancelled() => None,
        fatal = fatal_rx.recv() => {
            preparation_cancel.cancel();
            let _ = prep_task.await;
            client_task.abort();
            return Err(fatal.unwrap_or_else(|| anyhow::anyhow!("relay failure report was lost"))
                .context("relay durable state became unwritable"));
        }
        accepted = &mut client_task => {
            preparation_cancel.cancel();
            let _ = prep_task.await;
            let error = accepted
                .context("worker proxy accept task stopped")?
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("worker proxy accept task stopped"));
            return Err(error);
        }
        _ = &mut ctrl_c => {
            preparation_cancel.cancel();
            let _ = prep_task.await;
            client_task.abort();
            return Ok(());
        }
        _ = terminate.recv() => {
            preparation_cancel.cancel();
            let _ = prep_task.await;
            client_task.abort();
            return Ok(());
        }
    };

    let mut idle_dispatch_wakes = None;
    let running_harness = match preparation_result {
        Some(Ok(completion)) => {
            idle_dispatch_wakes = completion.idle_dispatch_wakes;
            match completion.result {
                Ok(running) => Some(running),
                Err(error) => {
                    tracing::warn!(%error, "harness preparation failed; keeping relay available");
                    None
                }
            }
        }
        Some(Err(error)) => {
            client_task.abort();
            return Err(anyhow::anyhow!("harness preparation task stopped: {error}"));
        }
        None => {
            // The supervisor publishes Failed before it returns from cancellation.
            if let Ok(completion) = (&mut prep_task).await {
                idle_dispatch_wakes = completion.idle_dispatch_wakes;
                if let Ok(running) = completion.result {
                    shutdown_running_harness(running).await;
                }
            }
            None
        }
    };

    let Some(running_harness) = running_harness else {
        // Keep the wake channel open while a failed or closed relay remains
        // attachable. Its bounded queue coalesces requests; no ACP work is
        // dispatched until the worker has a coordinator.
        let _idle_dispatch_wakes = idle_dispatch_wakes;
        tokio::select! {
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
    };

    let RunningHarness {
        mut acp_task,
        mut event_task,
        acp_shutdown,
        commands: acp_commands_tx,
        reviewer,
        subagent_socket_guard,
        harness_gc,
        shell_cleanup,
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
    shell_cleanup.close();
    shell_cleanup.wait().await;
    reviewer.pause_all().await;
    drop(subagent_socket_guard);
    if let Some(task) = harness_gc {
        task.abort();
    }
    if !closed {
        client_task.abort();
        return acp_result;
    }

    // Closed relays remain attachable so the controller can finish its
    // checkpoint. The connection handler rejects harness-only operations.
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
    relay: Arc<Mutex<DurableRelay>>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    runtime: ConnectionRuntime,
    fatal: mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await.context("accept worker proxy")?;
        let client_relay = relay.clone();
        let client_dispatch_wake = dispatch_wake.clone();
        let client_credentials = credentials.clone();
        let client_runtime = runtime.clone();
        let client_fatal = fatal.clone();
        tokio::spawn(async move {
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
    // On timeout or cancellation this detaches the blocking thread; worker
    // process teardown reclaims it if the call never returns.
    let step_cancel = cancel.child_token();
    let operation_cancel = step_cancel.clone();
    let task = tokio::task::spawn_blocking(move || operation(operation_cancel));
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

async fn supervise_harness_preparation(
    setup: HarnessPreparationSetup,
    dispatch_wake_rx: mpsc::Receiver<()>,
    status: watch::Sender<PreparationSnapshot>,
    cancel: CancellationToken,
) -> HarnessPreparationCompletion {
    let root = setup.root.clone();
    let mut idle_dispatch_wakes = Some(dispatch_wake_rx);
    let result = supervise_preparation(
        &root,
        &status,
        &cancel,
        prepare_and_start_harness(setup, &mut idle_dispatch_wakes, &status, &cancel),
    )
    .await;
    HarnessPreparationCompletion {
        result,
        idle_dispatch_wakes,
    }
}

async fn supervise_preparation<T>(
    root: &std::path::Path,
    status: &watch::Sender<PreparationSnapshot>,
    cancel: &CancellationToken,
    preparation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("{PREPARATION_CANCELLED_ERROR}")),
        result = catch_preparation_panic(preparation) => match result {
            Ok(result) => result,
            Err(error) => Err(error),
        },
    };
    if let Err(error) = &result {
        let snapshot = status.borrow().clone();
        let step = match snapshot.state {
            Some(mj_core::relay::HarnessPreparation::Preparing { step, .. })
            | Some(mj_core::relay::HarnessPreparation::Failed { step, .. }) => step,
            _ => "bridge-start".to_owned(),
        };
        let message = if cancel.is_cancelled() {
            PREPARATION_CANCELLED_ERROR.to_owned()
        } else {
            format!("{error:#}")
        };
        status.send_replace(PreparationSnapshot {
            state: Some(mj_core::relay::HarnessPreparation::Failed {
                step: step.clone(),
                error: message.clone(),
                at_ms: chrono::Utc::now().timestamp_millis(),
            }),
            services: None,
        });
        let root = root.to_owned();
        let record_step = step.clone();
        let record_message = message.clone();
        let record = tokio::task::spawn_blocking(move || {
            super::record_startup_failure(&root, &record_step, &record_message);
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
    }
    result
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

async fn prepare_and_start_harness(
    setup: HarnessPreparationSetup,
    dispatch_wake_rx: &mut Option<mpsc::Receiver<()>>,
    status: &watch::Sender<PreparationSnapshot>,
    cancel: &CancellationToken,
) -> Result<RunningHarness> {
    let HarnessPreparationSetup {
        root: root_path,
        mut config,
        relay,
        session_git_config_include,
        credentials,
        kimi_task_home,
        relay_state_exists,
        untracked_at_start,
        resume_session,
        native_session_may_have_history,
    } = setup;
    let subagent_role = config.subagent_mcp_role();
    let root = root_path.as_path();
    // The first capture must precede the primary harness, so pre-existing
    // edits are not reported as this session's turn. A restart retains its
    // original baseline and untracked list.
    if config.review_capture {
        let budget = preparation_step(
            root,
            status,
            "review-baseline",
            LOCAL_PREPARATION_STEP_TIMEOUT,
            cancel,
        )
        .await?;
        let record = root.join(REVIEW_UNTRACKED_FILE);
        let recorded: std::collections::BTreeMap<
            PathBuf,
            Vec<crate::review::capture::UntrackedEntry>,
        > = if relay_state_exists {
            let read_record = record.clone();
            bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                if step_cancel.is_cancelled() {
                    anyhow::bail!("preparation cancelled before reading review baseline");
                }
                Ok(std::fs::read(&read_record)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .unwrap_or_default())
            })
            .await?
        } else {
            let mut workspace_roots = vec![config.cwd.clone()];
            workspace_roots.extend(reviewer_workspace_directories(&config));
            bounded_preparation_step(&budget, async move {
                let git = crate::review::capture::BoundedGit::new(
                    crate::review::capture::WORKSPACE_STATE_TIMEOUT,
                );
                let _cancel_git = git.cancellation_guard();
                tokio::task::spawn_blocking(move || {
                    let repositories =
                        crate::review::capture::discover_repositories(&git, &workspace_roots);
                    crate::review::capture::initialize_review_baselines(&git, &repositories)
                })
                .await
                .map_err(|error| {
                    anyhow::anyhow!("review baseline initialization stopped: {error}")
                })?
            })
            .await?
        };
        let untracked_root = record;
        let untracked_at_start = untracked_at_start.clone();
        bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
            if step_cancel.is_cancelled() {
                anyhow::bail!("preparation cancelled before recording review baseline");
            }
            if let Ok(bytes) = serde_json::to_vec_pretty(&recorded) {
                let _ = mj_core::config::atomic_write(&untracked_root, &bytes);
            }
            *untracked_at_start
                .lock()
                .expect("untracked-at-start lock poisoned") = recorded;
            Ok(())
        })
        .await?;
    }

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
            mj_core::login_environment::resolve(),
        )
        .await
        .context("login environment resolution exceeded its 10-second bound")?
    })
    .await?;
    let mut session_environment = base_environment.clone();
    session_environment.extend(config.environment.clone());
    session_environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    session_environment.remove(mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV);
    session_environment.remove(mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV);
    let github_root = root.to_owned();
    let github_home = worker_home.clone();
    let git_config_include = session_git_config_include.clone();
    let mut explicit_environment = config.environment.clone();
    explicit_environment.remove(mj_core::worker_launch::SESSION_MANAGED_SUBAGENT_ENV);
    explicit_environment.remove(mj_core::worker_launch::SESSION_MESSAGE_MCP_ENV);
    let base_environment_for_filter = base_environment.clone();
    session_environment =
        bounded_blocking_preparation_step(&login_budget, cancel, move |step_cancel| {
            if step_cancel.is_cancelled() {
                anyhow::bail!("preparation cancelled before resolving Git credentials");
            }
            configure_github_cli(
                &github_root,
                &mut session_environment,
                github_home.as_deref(),
                git_config_include.as_deref(),
            )?;
            session_environment.retain(|name, value| {
                explicit_environment.contains_key(name)
                    || base_environment_for_filter.get(name) != Some(value)
            });
            Ok(session_environment)
        })
        .await?;
    // Persist only explicit and Mjolnir-generated overrides, never shell exports.
    config.environment = session_environment.clone();

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
            let mut environment = config.environment.clone();
            let role = subagent_role;
            let policy = config.execution_policy;
            let mailboxes_enabled = config.agent_mailboxes_enabled;
            let (registered, environment) =
                bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                    if step_cancel.is_cancelled() {
                        bail!("preparation cancelled before configuring the Codex profile");
                    }
                    let registered = super::subagents::configure_codex_profile(
                        &root,
                        &home,
                        &mut environment,
                        role,
                        policy,
                        mailboxes_enabled,
                    )?;
                    Ok((registered, environment))
                })
                .await?;
            config.environment = environment;
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
            let configure_subagents = subagent_role.is_some();
            let registration_required =
                subagent_role != Some(mj_core::subagent::SubagentMcpRole::MessageOnly);
            let mailboxes_enabled = config.agent_mailboxes_enabled;
            bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
                if step_cancel.is_cancelled() {
                    bail!("preparation cancelled before configuring the Claude profile");
                }
                if configure_subagents {
                    super::subagents::resolve_claude_mcp_paths(
                        &root,
                        &home,
                        registration_required,
                    )?;
                }
                super::subagents::configure_claude_mailbox_hook(&root, &home, mailboxes_enabled)?;
                Ok(true)
            })
            .await?
        }
        _ => false,
    };
    if config.project_memory.is_some()
        && resume_session.is_none()
        && config.harness != HarnessKind::Claude
    {
        let budget = preparation_step(
            root,
            status,
            "project-memory",
            LOCAL_PREPARATION_STEP_TIMEOUT,
            cancel,
        )
        .await?;
        let memory = config.project_memory.clone();
        let relay = relay.clone();
        bounded_blocking_preparation_step(&budget, cancel, move |step_cancel| {
            if step_cancel.is_cancelled() {
                bail!("preparation cancelled before loading project memory");
            }
            let should_install = relay
                .lock()
                .expect("relay state lock poisoned")
                .operational_state()
                .native_session_id
                .is_none();
            if should_install && let Some(memory) = memory {
                let store = mj_core::project_memory::ProjectMemoryStore::new(&memory.root);
                let text = mj_core::project_memory::startup_prompt_context(
                    &store,
                    &memory.repository_roots,
                )?;
                if step_cancel.is_cancelled() {
                    bail!("preparation cancelled before installing project memory");
                }
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .install_prompt_context(text)?;
            }
            Ok(())
        })
        .await?;
    }

    let harness_budget = preparation_step(
        root,
        status,
        "harness-resolve",
        std::time::Duration::from_secs(20 * 60),
        cancel,
    )
    .await?;
    let prepared_harness = bounded_preparation_step(
        &harness_budget,
        super::prepare_harness_launch(
            config.harness,
            config.harness_runtime,
            config.execution_policy,
            AcpSupervisorSpec::from(&config),
        ),
    )
    .await?;
    let managed_cache_root = prepared_harness
        .managed
        .as_ref()
        .map(|managed| managed.cache_root.clone());
    let _managed_harness_lease = prepared_harness.managed;

    let acp_budget = preparation_step(
        root,
        status,
        "acp-setup",
        LOCAL_PREPARATION_STEP_TIMEOUT,
        cancel,
    )
    .await?;
    let acp_setup = AcpPreparationSetup {
        root: root.to_owned(),
        config,
        session_environment: prepared_harness.environment,
        supervisor_spec: prepared_harness.spec,
        relay: relay.clone(),
        untracked_at_start,
        resume_session,
        native_session_may_have_history,
        profile_registration,
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

    let bridge_budget = preparation_step(
        root,
        status,
        "bridge-start",
        LOCAL_PREPARATION_STEP_TIMEOUT,
        cancel,
    )
    .await?;
    let dispatch_wakes = dispatch_wake_rx
        .take()
        .context("preparation lost ownership of relay dispatch wake channel")?;
    let relay = relay.clone();
    let bridge_runtime = tokio::runtime::Handle::current();
    let bridge_cancel = cancel.clone();
    // This stage only schedules prepared async tasks on the captured runtime;
    // it performs no filesystem or process I/O and cannot block a runtime
    // thread. The bounded future keeps cancellation ahead of task creation.
    let started = bounded_preparation_step(&bridge_budget, async move {
        if bridge_cancel.is_cancelled() {
            bail!("preparation cancelled before bridge start");
        }
        start_bridge(
            prepared_acp,
            relay,
            dispatch_wakes,
            kimi_task_home,
            bridge_runtime,
            bridge_cancel,
        )
    })
    .await?;
    let running = started.into_running();
    status.send_replace(PreparationSnapshot {
        state: Some(mj_core::relay::HarnessPreparation::Started),
        services: Some(Arc::new(PreparedConnectionServices {
            commands: running.commands.clone(),
            reviewer: running.reviewer.clone(),
            subagents: running.subagents.clone(),
        })),
    });

    Ok(running)
}

async fn shutdown_running_harness(mut running: RunningHarness) {
    running.acp_shutdown.cancel();
    stop_failed_coordinator(&mut running.event_task).await;
    if let Err(error) = running.acp_task.await {
        tracing::warn!(%error, "ACP runtime stopped while cancelling harness preparation");
    }
    running.shell_cleanup.close();
    running.shell_cleanup.wait().await;
    running.reviewer.pause_all().await;
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
    environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    environment.extend(config.environment);
    environment.remove(mj_core::worker_launch::SESSION_GIT_CONFIG_INCLUDE_PATH);
    config.environment = environment;
    if config.requires_harness_preparation() {
        super::prepare_harness_launch(
            config.harness,
            config.harness_runtime,
            config.execution_policy,
            AcpSupervisorSpec::from(&config),
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn serve_terminal_relay(
    listener: UnixListener,
    relay: Arc<Mutex<DurableRelay>>,
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
        tokio::spawn(async move {
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
    pub(super) project_memory: ProjectMemoryEndpoint,
    /// The ACP coordinator's command channel, or `None` once the session is
    /// sealed and no ACP runtime is left to serve scratch prompts.
    pub(super) commands: Option<mpsc::Sender<CommandRequest>>,
    /// The second-opinion reviewer, when this worker has an ACP runtime to run
    /// one beside. A sealed session has none.
    pub(super) reviewer: Option<Arc<ReviewerSidecar>>,
    pub(super) subagents: Option<super::subagents::SubagentEndpoint>,
    pub(super) cpu: Option<tokio::sync::watch::Receiver<crate::cpu_usage::CpuRead>>,
    pub(super) preparation: Option<watch::Receiver<PreparationSnapshot>>,
    pub(super) preparation_cancel: Option<CancellationToken>,
    pub(super) has_subagent_tools: bool,
}

#[cfg(test)]
pub(super) async fn serve_client(
    stream: UnixStream,
    relay: Arc<Mutex<DurableRelay>>,
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

/// Test-only relay entry point that exposes the worker's reviewer sidecar.
/// The production daemon supplies the same `ConnectionRuntime` from its live
/// ACP setup; keeping this seam beside `serve_client` lets socket tests drive
/// the real reviewer cancellation path without opening a second journal.
#[cfg(test)]
pub(super) async fn serve_client_with_reviewer(
    stream: UnixStream,
    relay: Arc<Mutex<DurableRelay>>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    commands: Option<mpsc::Sender<CommandRequest>>,
    reviewer: Arc<ReviewerSidecar>,
    fatal: mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    serve_client_with_memory(
        stream,
        relay,
        dispatch_wake,
        credentials,
        ConnectionRuntime {
            commands,
            reviewer: Some(reviewer),
            ..ConnectionRuntime::default()
        },
        fatal,
    )
    .await
}

pub(super) async fn serve_client_with_memory(
    stream: UnixStream,
    relay: Arc<Mutex<DurableRelay>>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    runtime: ConnectionRuntime,
    fatal: mpsc::Sender<anyhow::Error>,
) -> Result<()> {
    let ConnectionRuntime {
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
                .or_else(|| subagents.clone());
            if matches!(
                &envelope.request,
                RelayRequest::AttachmentPresent { .. }
                    | RelayRequest::InstallAttachment { .. }
                    | RelayRequest::ReadAttachment { .. }
            ) {
                let operation = envelope.request.method_name();
                let request_id = envelope.request_id.clone();
                let protocol_version = envelope.protocol_version;
                let store = mj_core::attachment::AttachmentStore::worker(&relay_root);
                let body =
                    match tokio::task::spawn_blocking(move || -> Result<RelayResponsePayload> {
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
                            &format!("image attachment task failed: {error}"),
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
            if matches!(
                &envelope.request,
                RelayRequest::CredentialState
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
                let response = credential_response(envelope, &credentials, &relay_root).await;
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if matches!(
                &envelope.request,
                RelayRequest::ProjectMemorySnapshot
                    | RelayRequest::InstallProjectMemorySnapshot { .. }
                    | RelayRequest::ReplaceProjectMemoryTree { .. }
            ) {
                let operation = envelope.request.method_name();
                let response = project_memory_response(envelope, &project_memory).await;
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if matches!(&envelope.request, RelayRequest::HistoryQuery { .. } | RelayRequest::HistoryRequests | RelayRequest::CompleteHistoryRequest { .. }) {
                let operation = envelope.request.method_name();
                let result: Result<RelayResponsePayload> = match envelope.request {
                    RelayRequest::HistoryRequests => Ok(RelayResponsePayload::HistoryRequests { requests: project_memory.history.requests() }),
                    RelayRequest::CompleteHistoryRequest { result } => {
                        project_memory.history.complete(result);
                        Ok(RelayResponsePayload::HistoryRequestCompleted)
                    }
                    RelayRequest::HistoryQuery { query } => {
                        if project_memory.config.as_ref().is_none_or(|config| config.history_socket.is_none()) {
                            Err(anyhow::anyhow!("history tools are unavailable in this worker; resume with a current controller"))
                        } else {
                            tokio::select! {
                                result = project_memory.history.query(query, &project_memory.cwd) => result.map(|result| RelayResponsePayload::HistoryResult { result }),
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
                // The reviewer is a sidecar with its own relay and its own
                // harness process. Both live on this connection's worker, so
                // the primary's relay never sees these.
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
                        "session is closed; no reviewer can run beside it".into(),
                    )
                } else {
                    reviewer_response(envelope, request_reviewer.as_ref(), &mut reader).await
                };
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if matches!(
                &envelope.request,
                RelayRequest::SubagentRequests
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
                    (Some(endpoint), RelayRequest::SubagentRequests) => {
                        let (requests, results) = endpoint.collect_for_daemon();
                        RelayResponseBody::Ok {
                            payload: RelayResponsePayload::SubagentRequests { requests, results },
                        }
                    }
                    (Some(endpoint), RelayRequest::CompleteSubagentRequest { result }) => {
                        match endpoint.complete(result) {
                            Ok(delivered_to_waiter) => RelayResponseBody::Ok {
                                payload: if protocol_version
                                    >= mj_core::relay::RELAY_PROTOCOL_VERSION
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
                    (None, RelayRequest::SubagentRequests) => RelayResponseBody::Ok {
                        payload: RelayResponsePayload::SubagentRequests {
                            requests: Vec::new(),
                            results: Vec::new(),
                        },
                    },
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
            let wakes_dispatch = matches!(
                &envelope.request,
                RelayRequest::Submit { .. } | RelayRequest::ReserveIdle { .. }
            );
            let mailbox_lease_changed = matches!(
                &envelope.request,
                RelayRequest::DrainMailbox { .. } | RelayRequest::AckMailbox { .. }
            );
            let checkpoint_change = checkpoint_change(&envelope.request);
            let close_request = matches!(
                &envelope.request,
                RelayRequest::Submit {
                    command: RelayCommand::Close { .. },
                    ..
                }
            );
            let operation = envelope.request.method_name();
            let mut response = match handle_request(&relay, envelope).await {
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
            let current_preparation = preparation
                .as_ref()
                .map(|receiver| receiver.borrow().clone())
                .unwrap_or_default();
            let preparation_pending =
                preparation.is_some() && current_preparation.is_pending();
            if preparation_pending {
                relay
                    .lock()
                    .expect("relay state lock poisoned")
                    .dispatch_preparation_lifecycle()?;
            }
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
                }
            );
            let execution = relay
                .lock()
                .expect("relay state lock poisoned")
                .operational_state()
                .execution;
            if close_request
                && preparation_pending
                && matches!(
                    execution,
                    mj_core::relay::RelayExecutionState::Closing
                        | mj_core::relay::RelayExecutionState::Closed
                )
                && let Some(cancel) = &preparation_cancel
            {
                cancel.cancel();
            }
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
            if (wakes_dispatch && accepted) || mailbox_lease_changed {
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
mod reviewer_disconnect_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn finishing_an_operation_preserves_a_partially_buffered_next_frame() {
        let (mut client, server) = tokio::io::duplex(4096);
        let payload = "x".repeat(128 * 1024);
        let expected = payload.clone();
        let writer = tokio::spawn(async move {
            client.write_all(payload.as_bytes()).await.unwrap();
            client.write_all(b"\n").await.unwrap();
        });
        let mut reader = BufReader::new(server);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                wait_for_reviewer_disconnect(&mut reader)
            )
            .await
            .is_err(),
            "buffered input is not a disconnect"
        );
        assert_eq!(
            read_bounded_line(&mut reader, 256 * 1024).await.unwrap(),
            Some(expected)
        );
        writer.await.unwrap();
        assert_eq!(
            wait_for_reviewer_disconnect(&mut reader).await,
            ReviewerCancellation::ClientDisconnected
        );
    }
}

#[cfg(test)]
mod preparation_tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
    use mj_core::relay::{RELAY_EVENT_GENESIS_DIGEST, RelayCursor, RelayVersionRange};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    type TestReader = BufReader<OwnedReadHalf>;

    fn test_relay(root: &std::path::Path) -> Arc<Mutex<DurableRelay>> {
        Arc::new(Mutex::new(
            DurableRelay::open(root, "preparation-test", "1.0.0").unwrap(),
        ))
    }

    async fn start_control_server(
        root: &std::path::Path,
        relay: Arc<Mutex<DurableRelay>>,
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
        let accept = tokio::spawn(accept_worker_clients(
            listener,
            guard,
            relay,
            dispatch_wake,
            Err("fixture has no credentials".into()),
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

    fn reviewer(relay: Arc<Mutex<DurableRelay>>, root: &std::path::Path) -> Arc<ReviewerSidecar> {
        Arc::new(ReviewerSidecar::new(
            ReviewerPlacement {
                target_environment: BTreeMap::new(),
                worker_root: root.to_path_buf(),
                session_id: "preparation-test".into(),
                cwd: root.to_path_buf(),
                additional_directories: Vec::new(),
                worker_executable: PathBuf::from("/bin/true"),
                harness_runtime: mj_core::worker_launch::HarnessRuntimePolicy::Ambient,
                review_capture: false,
                untracked_at_start: Arc::new(Mutex::new(BTreeMap::new())),
            },
            relay,
        ))
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
                reviewer: reviewer(relay.clone(), root),
                subagents: None,
            })),
        };
        let (preparation_started, started) = tokio::sync::oneshot::channel();
        let (release_preparation, release) = tokio::sync::oneshot::channel();
        let prep_root = root.to_path_buf();
        let prep_status = status.clone();
        let fake_root = prep_root.clone();
        let fake_status = prep_status.clone();
        let prep_cancellation = cancellation.clone();
        let fake_cancellation = prep_cancellation.clone();
        let prep = tokio::spawn(async move {
            supervise_preparation(&prep_root, &prep_status, &prep_cancellation, async move {
                preparation_step(
                    &fake_root,
                    &fake_status,
                    "harness-resolve",
                    LOCAL_PREPARATION_STEP_TIMEOUT,
                    &fake_cancellation,
                )
                .await?;
                let _ = preparation_started.send(());
                release.await.context("fake preparation gate was dropped")?;
                fake_status.send_replace(prepared);
                Ok(())
            })
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
        let coordinator = tokio::spawn(run_relay_coordinator_with_verdict(
            relay,
            event_stream,
            dispatch_wakes,
            commands,
            shells,
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

    #[tokio::test]
    async fn failed_preparation_stays_attachable_and_checkpointable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let relay = test_relay(root);
        let (status, preparation) = watch::channel(PreparationSnapshot::default());
        let cancellation = CancellationToken::new();
        let (dispatch_wake, _dispatch_wakes) = mpsc::channel(1);
        let (accept, mut reader, mut writer) = start_control_server(
            root,
            relay,
            dispatch_wake,
            preparation,
            cancellation.clone(),
        )
        .await;
        let prep_root = root.to_path_buf();
        let prep_status = status.clone();
        let prep_cancellation = cancellation.clone();
        let step_cancellation = prep_cancellation.clone();
        let error = supervise_preparation::<()>(root, &status, &prep_cancellation, async move {
            preparation_step(
                &prep_root,
                &prep_status,
                "harness-resolve",
                LOCAL_PREPARATION_STEP_TIMEOUT,
                &step_cancellation,
            )
            .await?;
            anyhow::bail!("fake pinned install failed")
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("fake pinned install failed"));

        let attached = request(
            &mut reader,
            &mut writer,
            "attach-after-failure",
            RelayRequest::Attach {
                after_ordinal: 0,
                after_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            },
        )
        .await;
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::Attached { state, .. },
        } = attached.body
        else {
            panic!("relay stopped serving Attach after preparation failed");
        };
        assert!(matches!(
            state.harness_preparation,
            Some(mj_core::relay::HarnessPreparation::Failed { ref step, ref error, .. })
                if step == "harness-resolve" && error.contains("fake pinned install failed")
        ));
        let startup: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(super::super::WORKER_STARTUP_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(startup["failure"]["step"], "harness-resolve");
        assert!(
            startup["failure"]["error"]
                .as_str()
                .unwrap()
                .contains("fake pinned install failed")
        );

        let checkpoint = request(
            &mut reader,
            &mut writer,
            "checkpoint",
            RelayRequest::Submit {
                command_id: "checkpoint-during-failure".into(),
                command: RelayCommand::BeginCheckpoint { reason: None },
            },
        )
        .await;
        assert!(matches!(checkpoint.body, RelayResponseBody::Ok { .. }));
        let completed = request(
            &mut reader,
            &mut writer,
            "complete-after-failure",
            RelayRequest::Submit {
                command_id: "complete-after-failure".into(),
                command: RelayCommand::CompleteCheckpoint {
                    barrier_command_id: "checkpoint-during-failure".into(),
                },
            },
        )
        .await;
        assert!(matches!(completed.body, RelayResponseBody::Ok { .. }));
        let prompt = request(
            &mut reader,
            &mut writer,
            "prompt-after-failure",
            RelayRequest::Submit {
                command_id: "prompt-after-failure".into(),
                command: RelayCommand::Prompt {
                    prompt: vec![ContentBlock::Text(TextContent::new("queue after failure"))],
                },
            },
        )
        .await;
        assert!(matches!(prompt.body, RelayResponseBody::Ok { .. }));
        let status_response = request(
            &mut reader,
            &mut writer,
            "status-after-checkpoint",
            RelayRequest::Status,
        )
        .await;
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::Status(state),
        } = status_response.body
        else {
            panic!("status unavailable after failed preparation");
        };
        assert!(state.checkpoint_ready.is_none());
        assert!(
            state
                .active_prompt
                .as_ref()
                .is_some_and(|prompt| prompt.command_id == "prompt-after-failure")
                || state
                    .queued_prompts
                    .iter()
                    .any(|prompt| prompt.command_id == "prompt-after-failure"),
            "accepted prompt was not represented in relay state: {state:?}"
        );
        writer.shutdown().await.unwrap();
        accept.abort();
    }

    #[tokio::test]
    async fn panicking_preparation_publishes_a_terminal_failure() {
        fn panic_fake_preparation() -> Result<()> {
            panic!("fake profile step panicked");
        }

        let temp = tempfile::tempdir().unwrap();
        let (status, _preparation) = watch::channel(PreparationSnapshot {
            state: Some(mj_core::relay::HarnessPreparation::Preparing {
                step: "subagent-profile".into(),
                since_ms: chrono::Utc::now().timestamp_millis(),
            }),
            services: None,
        });
        let cancellation = CancellationToken::new();
        let result = supervise_preparation::<()>(temp.path(), &status, &cancellation, async {
            panic_fake_preparation()
        })
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("fake profile step panicked")
        );
        assert!(matches!(
            status.borrow().state.as_ref(),
            Some(mj_core::relay::HarnessPreparation::Failed {
                step,
                error,
                ..
            }) if step == "subagent-profile" && error.contains("fake profile step panicked")
        ));
    }

    #[tokio::test]
    async fn blocking_preparation_deadline_fails_step_while_hello_and_attach_keep_working() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let relay = test_relay(root);
        let (status, preparation) = watch::channel(PreparationSnapshot::default());
        let cancellation = CancellationToken::new();
        let (dispatch_wake, _dispatch_wakes) = mpsc::channel(1);
        let (accept, mut reader, mut writer) = start_control_server(
            root,
            relay,
            dispatch_wake,
            preparation,
            cancellation.clone(),
        )
        .await;
        let prep_root = root.to_path_buf();
        let prep_status = status.clone();
        let prep_cancellation = cancellation.clone();
        let status_for_step = prep_status.clone();
        let cancellation_for_step = prep_cancellation.clone();
        let (blocking_started_tx, blocking_started_rx) = tokio::sync::oneshot::channel();
        let (release_blocking_tx, release_blocking_rx) = std::sync::mpsc::channel();
        let (blocking_finished_tx, blocking_finished_rx) = std::sync::mpsc::channel();
        let prep = tokio::spawn(async move {
            supervise_preparation(&prep_root, &prep_status, &prep_cancellation, async move {
                status_for_step.send_replace(PreparationSnapshot {
                    state: Some(mj_core::relay::HarnessPreparation::Preparing {
                        step: "acp-setup".into(),
                        since_ms: chrono::Utc::now().timestamp_millis(),
                    }),
                    services: None,
                });
                let budget = PreparationStepBudget::new("acp-setup", Duration::from_millis(200));
                bounded_blocking_preparation_step(&budget, &cancellation_for_step, move |_| {
                    let _ = blocking_started_tx.send(());
                    release_blocking_rx
                        .recv()
                        .context("test did not release blocked preparation")?;
                    let _ = blocking_finished_tx.send(());
                    Ok(())
                })
                .await
            })
            .await
        });
        blocking_started_rx.await.unwrap();
        let preparation_result = tokio::time::timeout(Duration::from_secs(2), prep)
            .await
            .expect("the injected preparation deadline should expire")
            .unwrap();
        let error = preparation_result.unwrap_err();
        assert!(error.to_string().contains("acp-setup"));
        assert!(error.to_string().contains("deadline"));
        assert!(matches!(
            status.borrow().state.as_ref(),
            Some(mj_core::relay::HarnessPreparation::Failed {
                step,
                error,
                ..
            }) if step == "acp-setup" && error.contains("200ms deadline")
        ));
        assert!(matches!(
            blocking_finished_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        let hello = request(
            &mut reader,
            &mut writer,
            "hello-after-preparation-timeout",
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
            "attach-after-preparation-timeout",
            RelayRequest::Attach {
                after_ordinal: 0,
                after_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
            },
        )
        .await;
        let RelayResponseBody::Ok {
            payload: RelayResponsePayload::Attached { state, .. },
        } = attached.body
        else {
            panic!("Attach failed while timed-out setup thread was blocked");
        };
        assert!(matches!(
            state.harness_preparation,
            Some(mj_core::relay::HarnessPreparation::Failed { ref step, .. })
                if step == "acp-setup"
        ));
        assert!(matches!(
            blocking_finished_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        release_blocking_tx.send(()).unwrap();
        tokio::task::spawn_blocking(move || blocking_finished_rx.recv())
            .await
            .unwrap()
            .unwrap();
        writer.shutdown().await.unwrap();
        accept.abort();
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
        let prep = tokio::spawn(async move {
            supervise_preparation::<()>(&prep_root, &prep_status, &prep_cancellation, async move {
                preparation_step(
                    &fake_root,
                    &fake_status,
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
                let output =
                    mj_core::subprocess::run_bounded(&mut child, 1024, Duration::from_secs(120))
                        .await?;
                anyhow::bail!("fake harness unexpectedly exited: {}", output.status)
            })
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
        assert!(preparation_result.is_err());
        assert!(matches!(
            status.borrow().state.as_ref(),
            Some(mj_core::relay::HarnessPreparation::Failed {
                step,
                error,
                ..
            }) if step == "harness-resolve" && error.contains("closing")
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
        assert!(matches!(
            final_state.harness_preparation,
            Some(mj_core::relay::HarnessPreparation::Failed { ref error, .. })
                if error.contains("closing")
        ));
        drop(dispatch_wakes);
        writer.shutdown().await.unwrap();
        accept.abort();
    }
}
