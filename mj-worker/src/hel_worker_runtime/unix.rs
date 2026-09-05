use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1::SessionUpdate;
use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use super::reviewer::{ReviewerPlacement, ReviewerSidecar};
use super::{AcpSupervisorSpec, CredentialEndpoint, DISCOVER_LOGIN_PATH_ENV, WorkerLaunchConfig};

pub(super) const PROXY_INITIAL_INPUT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);
use hel::hel_acp::{self, CommandRequest, LaunchSpec, RuntimeEvent};
use hel::hel_config::HarnessKind;
use hel::hel_subprocess::terminate_process_group;
use hel::hel_worker::{
    ClaimedRelayCommand, DeferredRelayAttach, DurableRelay, RELAY_STATE_FILE,
    RESTORED_RELAY_SEED_FILE, RelayCommand, RelayCommandOutcome, RelayErrorCode, RelayObservation,
    RelayProtocolError, RelayRequest, RelayRequestEnvelope, RelayResponseBody,
    RelayResponseEnvelope, RelayResponsePayload, incompatible_request_protocol_response,
    invalid_relay_request_response, unsupported_relay_method_response,
};
use hel::hel_worker_protocol::{DecodedRelayRequest, decode_relay_request};

pub(crate) const ACP_EVENT_CHANNEL_CAPACITY: usize = 256;
const LOGIN_PATH_DISCOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LOGIN_PATH_MARKER: &str = "__HEL_LOGIN_PATH__=";

#[derive(Clone)]
pub(super) struct ProjectMemoryEndpoint {
    config: Option<super::ProjectMemoryLaunchConfig>,
    io: Arc<tokio::sync::Semaphore>,
}

impl ProjectMemoryEndpoint {
    fn new(config: Option<super::ProjectMemoryLaunchConfig>) -> Self {
        Self {
            config,
            io: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

impl Default for ProjectMemoryEndpoint {
    fn default() -> Self {
        Self::new(None)
    }
}

pub(super) struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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

pub async fn run_daemon(root: PathBuf, mut config: WorkerLaunchConfig) -> Result<()> {
    let startup_directory = std::env::current_dir()?;
    let root = super::resolve_relative_worker_root(root, &startup_directory);
    super::resolve_relative_harness_home(&mut config, &startup_directory);
    super::enforce_execution_policy(&mut config);
    // Resolve this before the launch config's environment is consumed by
    // the ACP supervisor specification below.
    let credentials = super::credential_endpoint(&config);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create worker root {}", root.display()))?;
    if config.environment.remove(DISCOVER_LOGIN_PATH_ENV).is_some()
        && !config.environment.contains_key("PATH")
        && let Some(path) = discover_login_path(&config.session_id).await
    {
        config.environment.insert("PATH".into(), path);
    }
    configure_github_cli(&root, &mut config.environment)?;
    let managed_harness = super::harness::resolve(
        config.harness_runtime,
        config.harness,
        config.execution_policy,
        &config.environment,
    )
    .await
    .with_context(|| format!("prepare managed {}", config.harness.display_name()))?;
    if let Some(managed) = &managed_harness {
        config.bridge_command = managed.command.clone();
        config.bridge_args = managed.args.clone();
        config.environment.extend(managed.environment.clone());
    }
    let harness_gc = managed_harness
        .as_ref()
        .map(|managed| super::harness::spawn_gc(managed.cache_root.clone(), config.harness));
    let socket = root.join("control.sock");
    // Refuse a second daemon before touching durable state: opening the
    // relay recovers the journal in place, so getting that far would
    // corrupt the files a live worker is still writing.
    if socket.exists() && UnixStream::connect(&socket).await.is_ok() {
        bail!("a worker is already running at {}", socket.display());
    }
    // A dead daemon can leave its socket inode behind. Remove it before
    // journal recovery so controller liveness can distinguish a worker
    // that is still starting from one that has published its endpoint.
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("remove stale socket {}", socket.display()))?;
    }
    let restarting =
        root.join(RELAY_STATE_FILE).exists() || root.join(RESTORED_RELAY_SEED_FILE).exists();
    // Validate and recover durable state before publishing a socket. A
    // failed startup must never leave a fresh endpoint that looks live.
    let mut durable_relay =
        DurableRelay::open(&root, &config.session_id, env!("CARGO_PKG_VERSION"))?;
    // Hashed once, at startup: the controller compares this against the binary
    // it would install to decide whether this worker is the current build.
    durable_relay.set_worker_build(hel::hel_worker_launch::running_executable_digest());
    // Only Claude Code's adapter marks the end of a turn it started on its
    // own, so only it can model those turns without leaving a session stuck
    // Running. See `.agents/docs/claude-autonomous-turns.md`.
    durable_relay.set_harness_turn_policy(match config.harness {
        HarnessKind::Claude => hel::hel_worker::HarnessTurnPolicy::ClaudeAdapter,
        _ => hel::hel_worker::HarnessTurnPolicy::Disabled,
    });
    // Codex runs its own shells instead of asking Hel for a terminal, so the
    // only evidence of a command it left running is a card with no exit code.
    durable_relay.set_background_work_policy(match config.harness {
        HarnessKind::Codex => hel::hel_worker::BackgroundWorkPolicy::CodexExecCards,
        _ => hel::hel_worker::BackgroundWorkPolicy::HostedTerminals,
    });
    let resume_session = select_resume_session(&config, &durable_relay);
    let project_memory = ProjectMemoryEndpoint::new(config.project_memory.clone());
    if resume_session.is_none()
        && config.harness != HarnessKind::Claude
        && let Some(memory) = &config.project_memory
    {
        let store = hel::hel_project_memory::ProjectMemoryStore::new(&memory.root);
        durable_relay.install_prompt_context(hel::hel_project_memory::startup_prompt_context(
            &store,
            &memory.repository_roots,
        )?)?;
    }
    // Startup succeeded far enough to own this root, so claim it. A failed
    // open leaves any previous pidfile alone rather than pointing teardown
    // at a process that never took over.
    write_worker_pidfile(&root, std::process::id())?;
    // Durable state recovered, so any exit record belongs to a previous
    // life of this worker. Leaving it would make the controller read this
    // startup as another death.
    let exit_record = root.join("worker-exit.json");
    if exit_record.exists() {
        std::fs::remove_file(&exit_record)
            .with_context(|| format!("clear stale exit record {}", exit_record.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("bind worker socket {}", socket.display()))?;
    let _socket_guard = SocketGuard(socket.clone());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }

    if restarting
        && durable_relay.operational_state().execution
            != hel::hel_worker::RelayExecutionState::Closed
    {
        durable_relay.record_observation(RelayObservation::SessionRestarted)?;
    }

    let relay = Arc::new(Mutex::new(durable_relay));
    // Client tasks are detached, so a durable failure they cannot recover
    // from has to travel back here to stop the daemon.
    let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
    if relay
        .lock()
        .expect("relay state lock poisoned")
        .operational_state()
        .execution
        == hel::hel_worker::RelayExecutionState::Closed
    {
        // A durable close is a seal, including across target or daemon
        // restarts. Keep the relay attachable so the controller can catch
        // up and complete its checkpoint, but never reopen the ACP session.
        let (dispatch_wake_tx, dispatch_wake_rx) = mpsc::channel(1);
        drop(dispatch_wake_rx);
        return serve_terminal_relay(
            listener,
            relay,
            dispatch_wake_tx,
            credentials,
            project_memory,
            fatal_tx,
            fatal_rx,
        )
        .await;
    }

    let (acp_commands_tx, acp_commands_rx) = mpsc::channel(32);
    let (acp_events_tx, acp_events_rx) = mpsc::channel(ACP_EVENT_CHANNEL_CAPACITY);
    let (dispatch_wake_tx, dispatch_wake_rx) = mpsc::channel(1);
    let user_shells = crate::hel_user_shell::UserShellRegistry::new(
        config.cwd.clone(),
        config.environment.clone(),
        acp_events_tx.clone(),
    );
    let supervisor_path = root.join("acp-supervisor.json");
    AcpSupervisorSpec {
        command: config.bridge_command,
        args: config.bridge_args,
        environment: config.environment,
        cwd: config.cwd.clone(),
        harness_lease: managed_harness
            .as_ref()
            .map(|managed| managed.lease_path.clone()),
    }
    .write_spec(&supervisor_path)?;
    let worker_executable = std::env::current_exe().context("locate Hel worker executable")?;
    // The reviewer shares this session's target and working directory and
    // nothing else. It stays idle until a controller asks for a second
    // opinion, so constructing it costs nothing.
    let reviewer = Arc::new(ReviewerSidecar::new(ReviewerPlacement {
        worker_root: root.clone(),
        session_id: config.session_id.clone(),
        cwd: config.cwd.clone(),
        additional_directories: config.additional_directories.clone(),
        worker_executable: worker_executable.clone(),
        harness_runtime: config.harness_runtime,
    }));
    // The review supervisor's dispatch tool talks to this worker over its own
    // socket inside the reviewer directory: an MCP server started by a harness
    // has no relay connection, and the dispatch is not session history.
    let dispatch_socket = serve_review_dispatch(&root, reviewer.clone())?;
    // Everything that can start a reviewer runs inside this block, so every
    // way out of it — including an error — passes through the pause below.
    // Stopping the reviewer's process group before this worker exits is what
    // keeps a harness from outliving the session it was reviewing for.
    // One acquisition: a guard taken inside the struct literal below would
    // live until the literal ends and deadlock the next one.
    let (acp_activity, step_clock) = {
        let relay = relay.lock().expect("relay lock poisoned");
        (relay.acp_activity_clock(), relay.step_clock())
    };
    let outcome = async {
        let acp_spec = LaunchSpec {
            command: worker_executable,
            args: vec![
                "worker".into(),
                "acp-supervisor".into(),
                "--spec".into(),
                supervisor_path.to_string_lossy().into_owned(),
            ],
            environment: Default::default(),
            cwd: config.cwd,
            additional_directories: config.additional_directories,
            extra_mcp_servers: Vec::new(),
            project_memory: config.project_memory,
            resume_session,
            harness: config.harness,
            execution_policy: config.execution_policy,
            acp_activity,
            step_clock,
        };
        let mut acp_task = tokio::spawn(hel_acp::run(acp_spec, acp_commands_rx, acp_events_tx));

        let event_relay = relay.clone();
        let mut event_task = tokio::spawn(run_relay_coordinator_with_shells(
            event_relay,
            acp_events_rx,
            dispatch_wake_rx,
            acp_commands_tx.clone(),
            user_shells,
        ));

        let acp_join = loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted.context("accept worker proxy")?;
                    let client_relay = relay.clone();
                    let client_dispatch_wake = dispatch_wake_tx.clone();
                    let client_credentials = credentials.clone();
                    let client_fatal = fatal_tx.clone();
                    let client_commands = acp_commands_tx.clone();
                    let client_project_memory = project_memory.clone();
                    let client_reviewer = reviewer.clone();
                    tokio::spawn(async move {
                        if let Err(error) = serve_client_with_memory(
                            stream,
                            client_relay,
                            client_dispatch_wake,
                            client_credentials,
                            ConnectionRuntime {
                                project_memory: client_project_memory,
                                commands: Some(client_commands),
                                reviewer: Some(client_reviewer),
                            },
                            client_fatal,
                        ).await {
                            tracing::warn!(%error, "relay proxy client disconnected");
                        }
                    });
                }
                fatal = fatal_rx.recv() => {
                    let error = fatal
                        .unwrap_or_else(|| anyhow::anyhow!("relay failure report was lost"));
                    event_task.abort();
                    drop(acp_commands_tx);
                    return abort_peer_and_return(
                        &mut acp_task,
                        error,
                        "relay durable state became unwritable",
                    ).await;
                }
                result = &mut event_task => {
                    match result {
                        Ok(Ok(())) => break acp_task.await,
                        Ok(Err(error)) => {
                            drop(acp_commands_tx);
                            return abort_peer_and_return(
                                &mut acp_task,
                                error,
                                "relay coordinator failed",
                            ).await;
                        }
                        Err(error) => {
                            drop(acp_commands_tx);
                            return abort_peer_and_return(
                                &mut acp_task,
                                anyhow::anyhow!(error),
                                "relay coordinator task stopped",
                            ).await;
                        }
                    }
                }
                result = &mut acp_task => {
                    event_task.await.context("relay event task stopped")??;
                    break result;
                }
            }
        };
        let acp_result = match acp_join {
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
        };
        let closed = relay
            .lock()
            .expect("relay state lock poisoned")
            .operational_state()
            .execution
            == hel::hel_worker::RelayExecutionState::Closed;
        if !closed {
            return acp_result;
        }
        if let Err(error) = &acp_result {
            tracing::warn!(%error, "ACP runtime failed after the relay closed");
        }
        serve_terminal_relay(
            listener,
            relay,
            dispatch_wake_tx,
            credentials,
            project_memory,
            fatal_tx,
            fatal_rx,
        )
        .await
    }
    .await;
    reviewer.pause_all().await;
    drop(dispatch_socket);
    if let Some(task) = harness_gc {
        task.abort();
    }
    outcome
}

/// Install or validate the managed harness named by a proposed launch config
/// without starting, stopping, or otherwise touching the session worker.
pub async fn prepare_managed_harness(mut config: WorkerLaunchConfig) -> Result<()> {
    if config.environment.remove(DISCOVER_LOGIN_PATH_ENV).is_some()
        && !config.environment.contains_key("PATH")
        && let Some(path) = discover_login_path(&config.session_id).await
    {
        config.environment.insert("PATH".into(), path);
    }
    let prepared = super::harness::resolve(
        config.harness_runtime,
        config.harness,
        config.execution_policy,
        &config.environment,
    )
    .await
    .with_context(|| format!("prepare managed {}", config.harness.display_name()))?;
    if config.harness_runtime == hel::hel_worker_launch::HarnessRuntimePolicy::ManagedRemote
        && prepared.is_none()
    {
        bail!("managed remote harness preparation produced no installation");
    }
    Ok(())
}

async fn discover_login_path(session_id: &str) -> Option<String> {
    use hel::hel_targets::{BoundedProcessExecutor, CommandExecutor};

    let result = tokio::task::spawn_blocking(|| {
        BoundedProcessExecutor::new(LOGIN_PATH_DISCOVERY_TIMEOUT)
            .execute(&login_path_discovery_command())
    })
    .await;
    let output = match result {
        Ok(Ok(output)) if output.status == 0 => output,
        Ok(Ok(output)) => {
            tracing::warn!(
                session_id,
                status = output.status,
                "login PATH discovery failed; using the worker base PATH"
            );
            return None;
        }
        Ok(Err(error)) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "login PATH discovery failed; using the worker base PATH"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                session_id,
                %error,
                "login PATH discovery task stopped; using the worker base PATH"
            );
            return None;
        }
    };
    match parse_login_path(&output.stdout) {
        Some(path) => Some(path),
        None => {
            tracing::warn!(
                session_id,
                "login PATH discovery returned an invalid PATH; using the worker base PATH"
            );
            None
        }
    }
}

pub(super) fn login_path_discovery_command() -> hel::hel_targets::CommandSpec {
    hel::hel_targets::CommandSpec::new(
        "sh",
        [
            "-lc",
            &format!("printf '\\n{LOGIN_PATH_MARKER}%s\\n' \"$PATH\""),
        ],
    )
    .purpose("discover the target login PATH")
}

pub(super) fn parse_login_path(stdout: &[u8]) -> Option<String> {
    let output = std::str::from_utf8(stdout).ok()?;
    let path = output
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(LOGIN_PATH_MARKER))?;
    (!path.is_empty() && !path.chars().any(char::is_control)).then(|| path.to_owned())
}

pub(super) async fn abort_peer_and_return<T>(
    peer: &mut tokio::task::JoinHandle<T>,
    error: anyhow::Error,
    context: &'static str,
) -> Result<()> {
    peer.abort();
    let _ = peer.await;
    Err(error.context(context))
}

pub(super) async fn serve_terminal_relay(
    listener: UnixListener,
    relay: Arc<Mutex<DurableRelay>>,
    dispatch_wake: mpsc::Sender<()>,
    credentials: std::result::Result<CredentialEndpoint, String>,
    project_memory: ProjectMemoryEndpoint,
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
        let client_project_memory = project_memory.clone();
        tokio::spawn(async move {
            // A sealed session has no ACP runtime left, so compaction
            // cannot be served here.
            if let Err(error) = serve_client_with_memory(
                stream,
                client_relay,
                client_dispatch_wake,
                client_credentials,
                ConnectionRuntime {
                    project_memory: client_project_memory,
                    ..ConnectionRuntime::default()
                },
                client_fatal,
            )
            .await
            {
                tracing::warn!(%error, "closed relay proxy client disconnected");
            }
        });
    }
}

async fn run_relay_coordinator_with_shells(
    relay: Arc<Mutex<DurableRelay>>,
    mut events: mpsc::Receiver<RuntimeEvent>,
    mut dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
    mut user_shells: crate::hel_user_shell::UserShellRegistry,
) -> Result<()> {
    let mut in_flight = BTreeMap::new();
    let mut session_configured = false;
    dispatch_pending(
        &relay,
        &commands,
        &mut in_flight,
        session_configured,
        &mut user_shells,
    )?;
    let mut wakes_open = true;
    loop {
        tokio::select! {
            biased;
            wake = dispatch_wakes.recv(), if wakes_open => {
                if wake.is_none() {
                    wakes_open = false;
                } else {
                    // A wake only says durable work may now be available.
                    // Runtime events already in the channel always belong
                    // before any newly admitted checkpoint cut.
                    let queued = events.len();
                    if record_queued_runtime_events(
                        &relay,
                        &mut in_flight,
                        &mut events,
                        &mut session_configured,
                        &mut user_shells,
                        queued,
                    ).await? {
                        return Ok(());
                    }
                    dispatch_pending(
                        &relay,
                        &commands,
                        &mut in_flight,
                        session_configured,
                        &mut user_shells,
                    )?;
                }
            }
            event = events.recv() => {
                let Some(event) = event else {
                    interrupt_in_flight(
                        &relay,
                        &mut in_flight,
                        "ACP runtime stopped before the command completed",
                    )?;
                    return Ok(());
                };
                if record_runtime_event_batch(
                    &relay,
                    &mut in_flight,
                    event,
                    &mut events,
                    &mut session_configured,
                    &mut user_shells,
                ).await? {
                    return Ok(());
                }
                dispatch_pending(
                    &relay,
                    &commands,
                    &mut in_flight,
                    session_configured,
                    &mut user_shells,
                )?;
            }
        }
    }
}

/// A relay coordinator for a session that runs no user shells.
///
/// The reviewer sidecar is one: `!` commands belong to the person driving the
/// primary session, and are never routed to the agent reviewing its plan.
pub(crate) async fn run_relay_coordinator(
    relay: Arc<Mutex<DurableRelay>>,
    events: mpsc::Receiver<RuntimeEvent>,
    dispatch_wakes: mpsc::Receiver<()>,
    commands: mpsc::Sender<CommandRequest>,
) -> Result<()> {
    // The registry is inert without a live event sink, so its working
    // directory only has to exist.
    let (shell_events, _shell_events_rx) = mpsc::channel(1);
    let user_shells = crate::hel_user_shell::UserShellRegistry::new(
        std::env::current_dir()?,
        BTreeMap::new(),
        shell_events,
    );
    run_relay_coordinator_with_shells(relay, events, dispatch_wakes, commands, user_shells).await
}

/// Record the complete batch already emitted by the ACP runtime before
/// admitting a checkpoint barrier. A command event may itself materialize
/// several durable observations, and queued notification events belong to
/// the cut ahead of any waiting barrier.
async fn record_runtime_event_batch(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    first: RuntimeEvent,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configured: &mut bool,
    user_shells: &mut crate::hel_user_shell::UserShellRegistry,
) -> Result<bool> {
    track_user_shell_completion(user_shells, &first);
    if record_runtime_event_and_track_configuration(relay, in_flight, first, session_configured)? {
        return Ok(true);
    }
    let queued = events.len();
    record_queued_runtime_events(
        relay,
        in_flight,
        events,
        session_configured,
        user_shells,
        queued,
    )
    .await
}

async fn record_queued_runtime_events(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    session_configured: &mut bool,
    user_shells: &mut crate::hel_user_shell::UserShellRegistry,
    maximum: usize,
) -> Result<bool> {
    for recorded in 0..maximum {
        match events.try_recv() {
            Ok(event) => {
                track_user_shell_completion(user_shells, &event);
                if record_runtime_event_and_track_configuration(
                    relay,
                    in_flight,
                    event,
                    session_configured,
                )? {
                    return Ok(true);
                }
                if (recorded + 1) % 256 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => return Ok(false),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                interrupt_in_flight(
                    relay,
                    in_flight,
                    "ACP runtime stopped before the command completed",
                )?;
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn track_user_shell_completion(
    user_shells: &mut crate::hel_user_shell::UserShellRegistry,
    event: &RuntimeEvent,
) {
    if let RuntimeEvent::UserShellFinished { request_id, .. } = event {
        user_shells.completed(request_id);
    }
}

pub(super) fn record_runtime_event_and_track_configuration(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
    session_configured: &mut bool,
) -> Result<bool> {
    // A fresh bridge must configure its session again before any command is
    // dispatched to it. That gap is what lets the bridge drop the requests the
    // previous one left queued without racing a new dispatch.
    if matches!(event, RuntimeEvent::HarnessRestarting { .. }) {
        *session_configured = false;
    }
    *session_configured |= matches!(event, RuntimeEvent::SessionConfigured { .. });
    record_runtime_event(relay, in_flight, event)
}

pub(super) fn record_runtime_event(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    event: RuntimeEvent,
) -> Result<bool> {
    let stopped = matches!(event, RuntimeEvent::Stopped);
    let mut relay = relay.lock().expect("relay state lock poisoned");
    match event {
        RuntimeEvent::Connected {
            protocol_version: Some(protocol_version),
            capabilities: Some(capabilities),
            agent_info,
            ..
        } => {
            relay.record_observation(RelayObservation::AgentInitialized {
                protocol_version,
                capabilities,
                agent_info,
            })?;
        }
        RuntimeEvent::Connected { .. } => {
            relay.record_observation(RelayObservation::Warning {
                message: "ACP initialized without capability metadata".into(),
            })?;
        }
        RuntimeEvent::SessionStarted {
            native_session_id,
            resumed,
            ..
        } => {
            relay.record_observation(RelayObservation::SessionOpened {
                native_session_id,
                resumed,
            })?;
        }
        RuntimeEvent::SessionConfigured { config_options } => {
            relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
        }
        RuntimeEvent::SessionModesConfigured { modes } => {
            relay.record_observation(RelayObservation::SessionModesConfigured { modes })?;
        }
        RuntimeEvent::SessionUpdate { update } => {
            let typed = serde_json::from_value::<SessionUpdate>(update).map_err(|error| {
                anyhow::anyhow!("decode ACP session update for relay journal: {error}")
            });
            match typed {
                Ok(update) => {
                    relay.record_session_update(update)?;
                }
                Err(error) => {
                    relay.record_observation(RelayObservation::Warning {
                        message: format!("{error:#}"),
                    })?;
                    return Err(error);
                }
            }
        }
        RuntimeEvent::ElicitationRequested { request } => {
            relay.record_observation(RelayObservation::ElicitationRequested { request })?;
        }
        RuntimeEvent::ElicitationResolved {
            elicitation_id,
            action,
        } => {
            relay.record_observation(RelayObservation::ElicitationResolved {
                elicitation_id,
                action,
            })?;
        }
        RuntimeEvent::PromptFinished {
            request_id,
            stop_reason,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(
                &request_id,
                RelayCommandOutcome::Prompt { stop_reason },
            )?;
        }
        RuntimeEvent::ConfigApplied {
            request_id,
            key,
            value,
            config_options,
        } => {
            relay.record_observation(RelayObservation::ConfigurationUpdated { key, value })?;
            relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
            relay.record_command_completed(&request_id, RelayCommandOutcome::Configured)?;
            in_flight.remove(&request_id);
        }
        RuntimeEvent::SessionModeApplied {
            request_id,
            mode_id,
            config_options,
            modes,
        } => {
            relay.record_observation(RelayObservation::ConfigurationUpdated {
                key: "mode".to_owned(),
                value: mode_id,
            })?;
            relay.record_observation(RelayObservation::SessionConfigured { config_options })?;
            relay.record_observation(RelayObservation::SessionModesConfigured { modes })?;
            relay.record_command_completed(&request_id, RelayCommandOutcome::SessionModeSet)?;
            in_flight.remove(&request_id);
        }
        RuntimeEvent::CommandRejected {
            request_id,
            message,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_rejected(&request_id, message)?;
        }
        RuntimeEvent::CommandInterrupted {
            request_id,
            message,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_interrupted(&request_id, message)?;
        }
        RuntimeEvent::CancelApplied { request_id } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(&request_id, RelayCommandOutcome::Cancelled)?;
        }
        RuntimeEvent::SteerApplied {
            request_id,
            queued_command_id,
        } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(
                &request_id,
                RelayCommandOutcome::Steered { queued_command_id },
            )?;
        }
        RuntimeEvent::CloseApplied { request_id } => {
            in_flight.remove(&request_id);
            relay.record_command_completed(&request_id, RelayCommandOutcome::Closed)?;
            relay.record_observation(RelayObservation::Closed)?;
        }
        RuntimeEvent::Warning { message } => {
            relay.record_observation(RelayObservation::Warning { message })?;
        }
        RuntimeEvent::HarnessRestarting { message } => {
            relay.clear_agent_terminals();
            relay.record_observation(RelayObservation::Warning {
                message: message.clone(),
            })?;
            for (command_id, _) in std::mem::take(in_flight) {
                relay.record_command_interrupted(&command_id, message.clone())?;
            }
            relay.record_observation(RelayObservation::SessionRestarted)?;
        }
        RuntimeEvent::TerminalClosed {
            terminal_id,
            mut output,
            mut truncated,
            exit_code,
            signal,
        } => {
            relay.agent_terminal_closed(&terminal_id);
            // Cap here rather than letting `clamp_observation` fire: that
            // keeps the head of a string, and a terminal's tail is what
            // says how the command ended.
            truncated |= hel::hel_worker::truncate_start_with_marker(
                &mut output,
                hel::hel_worker::TERMINAL_JOURNAL_OUTPUT_BYTES,
            );
            relay.record_observation(RelayObservation::TerminalOutput {
                terminal_id,
                output,
                truncated,
                exit_code,
                signal,
            })?;
        }
        RuntimeEvent::TerminalStarted {
            terminal_id,
            command,
            started_at_ms,
        } => {
            relay.agent_terminal_started(hel::hel_worker::ActiveAgentTerminal {
                terminal_id: terminal_id.clone(),
                command: command.clone(),
                started_at_ms,
            });
            relay.record_session_update(SessionUpdate::ToolCall(
                hel::hel_acp::fallback_terminal_tool_call(&terminal_id, command),
            ))?;
        }
        RuntimeEvent::UserShellOutput {
            request_id,
            command,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        } => {
            relay.record_observation(RelayObservation::UserShellOutput {
                command_id: request_id,
                command,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
            })?;
        }
        RuntimeEvent::UserShellFinished { request_id, result } => {
            relay
                .record_command_completed(&request_id, RelayCommandOutcome::UserShell { result })?;
        }
        RuntimeEvent::Stopped => {
            relay.clear_agent_terminals();
            relay.record_observation(RelayObservation::ElicitationsCleared)?;
            if relay.operational_state().execution != hel::hel_worker::RelayExecutionState::Closed {
                relay.record_observation(RelayObservation::Warning {
                    message: "ACP runtime stopped".into(),
                })?;
            }
            for (command_id, _) in std::mem::take(in_flight) {
                relay.record_command_interrupted(
                    &command_id,
                    "ACP runtime stopped before the command completed",
                )?;
            }
        }
    }
    Ok(stopped)
}

fn interrupt_in_flight(
    relay: &Arc<Mutex<DurableRelay>>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    message: &str,
) -> Result<()> {
    let mut relay = relay.lock().expect("relay state lock poisoned");
    for (command_id, _) in std::mem::take(in_flight) {
        relay.record_command_interrupted(&command_id, message)?;
    }
    Ok(())
}

/// Hand durable work to the ACP runtime through capacity reserved before
/// the claim, so dispatch never waits. The command channel is shared with
/// out-of-band senders (compaction, elicitation answers); they now compete
/// for the same permits instead of stealing capacity a claim already
/// counted on. That matters because a coordinator parked on a send stops
/// draining ACP's bounded event channel, which stops the runtime that
/// would have drained these commands: a cycle nothing breaks.
///
/// This function deliberately holds no await point. A reserved permit
/// carries no durable state, so reserve -> durable claim -> permit send
/// keeps the claim-before-dispatch contract: a crash between the claim and
/// the send leaves the command in flight for restart interruption, exactly
/// as before.
fn dispatch_pending(
    relay: &Arc<Mutex<DurableRelay>>,
    commands: &mpsc::Sender<CommandRequest>,
    in_flight: &mut BTreeMap<String, RelayCommand>,
    session_configured: bool,
    user_shells: &mut crate::hel_user_shell::UserShellRegistry,
) -> Result<()> {
    let mut permits = Vec::new();
    // `Full` means dispatch what fits now and reserve again on the next
    // runtime event or wake. `Closed` means the ACP runtime is gone, so
    // claim nothing: leaving the commands pending lets the next run
    // dispatch them instead of interrupting work that never started, and
    // the closing events channel is what ends this coordinator.
    while let Ok(permit) = commands.try_reserve() {
        permits.push(permit);
    }
    dispatch_user_shells(relay, user_shells)?;
    let mut pending = relay
        .lock()
        .expect("relay state lock poisoned")
        .claim_pending_commands_up_to(session_configured, permits.len())?;
    pending.sort_by_key(|claimed| claimed.accepted_ordinal);
    // Hand back capacity the claim did not need before doing anything else.
    permits.truncate(pending.len());
    let mut permits = permits.into_iter();
    for claimed in pending {
        if matches!(&claimed.command, RelayCommand::BeginCheckpoint { .. }) {
            relay
                .lock()
                .expect("relay state lock poisoned")
                .record_checkpoint_ready(&claimed.command_id)?;
            continue;
        }
        let Some(command) = acp_command(&claimed) else {
            relay
                .lock()
                .expect("relay state lock poisoned")
                .record_command_rejected(
                    &claimed.command_id,
                    "relay-local command was unexpectedly claimed for ACP dispatch",
                )?;
            continue;
        };
        permits
            .next()
            .expect("every claimed command holds a reserved ACP command permit")
            .send(command);
        in_flight.insert(claimed.command_id, claimed.command);
    }
    Ok(())
}

fn dispatch_user_shells(
    relay: &Arc<Mutex<DurableRelay>>,
    user_shells: &mut crate::hel_user_shell::UserShellRegistry,
) -> Result<()> {
    let claimed = relay
        .lock()
        .expect("relay state lock poisoned")
        .claim_pending_user_shell_commands_up_to(user_shells.available_slots())?;
    for claimed in claimed {
        match claimed.command {
            RelayCommand::RunUserShell { command } => {
                if let Err(error) = user_shells.start(claimed.command_id.clone(), command.clone()) {
                    relay
                        .lock()
                        .expect("relay state lock poisoned")
                        .record_command_completed(
                            &claimed.command_id,
                            RelayCommandOutcome::UserShell {
                                result: hel::hel_worker::UserShellResult {
                                    command,
                                    stdout: String::new(),
                                    stderr: String::new(),
                                    stdout_truncated: false,
                                    stderr_truncated: false,
                                    exit_code: None,
                                    signal: None,
                                    duration_ms: 0,
                                    status: hel::hel_worker::UserShellStatus::Failed,
                                    error: Some(format!("{error:#}")),
                                },
                            },
                        )?;
                }
            }
            RelayCommand::CancelUserShell { shell_command_id } => {
                let cancellation = user_shells.cancel(&shell_command_id);
                let mut relay = relay.lock().expect("relay state lock poisoned");
                if cancellation == crate::hel_user_shell::UserShellCancelOutcome::NotRunning
                    && relay
                        .operational_state()
                        .active_user_shells
                        .iter()
                        .any(|shell| shell.command_id == shell_command_id)
                {
                    relay.record_command_interrupted(
                        &shell_command_id,
                        "shell command was cancelled before it started",
                    )?;
                }
                relay.record_command_completed(
                    &claimed.command_id,
                    RelayCommandOutcome::UserShellCancelled,
                )?;
            }
            _ => unreachable!("only user shell commands are claimed here"),
        }
    }
    Ok(())
}

fn acp_command(claimed: &ClaimedRelayCommand) -> Option<CommandRequest> {
    let request_id = claimed.command_id.clone();
    match &claimed.command {
        RelayCommand::Prompt { prompt } => {
            let mut prompt = prompt.clone();
            if let Some(context) = &claimed.hidden_prompt_context {
                prompt.insert(
                    0,
                    agent_client_protocol::schema::v1::ContentBlock::Text(
                        agent_client_protocol::schema::v1::TextContent::new(context.clone()),
                    ),
                );
            }
            Some(CommandRequest::Prompt { request_id, prompt })
        }
        RelayCommand::SetConfig { key, value } => Some(CommandRequest::SetConfig {
            request_id,
            key: key.clone(),
            value: value.clone(),
        }),
        RelayCommand::SetSessionMode { mode_id } => Some(CommandRequest::SetSessionMode {
            request_id,
            mode_id: mode_id.clone(),
        }),
        RelayCommand::Cancel => Some(CommandRequest::Cancel {
            request_id,
            steering_prompt: claimed.steering_prompt.clone(),
        }),
        RelayCommand::CancelTurn => Some(CommandRequest::Cancel {
            request_id,
            steering_prompt: None,
        }),
        RelayCommand::Close { .. } => Some(CommandRequest::Close { request_id }),
        RelayCommand::BeginCheckpoint { .. }
        | RelayCommand::RunUserShell { .. }
        | RelayCommand::CancelUserShell { .. }
        | RelayCommand::RemoveQueuedPrompt { .. }
        | RelayCommand::ClearQueuedPrompts
        | RelayCommand::CompleteCheckpoint { .. }
        | RelayCommand::ReleaseCheckpoint { .. }
        | RelayCommand::AdvanceRecoveryFloor { .. }
        | RelayCommand::RecordNotice { .. } => None,
    }
}

pub(super) fn select_resume_session(
    config: &WorkerLaunchConfig,
    relay: &DurableRelay,
) -> Option<String> {
    config
        .native_session_id
        .clone()
        .or_else(|| relay.operational_state().native_session_id)
}

pub(super) async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    maximum_bytes: usize,
) -> Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await.context("read relay request")?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(line)
                .context("relay request is not UTF-8")
                .map(Some);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let content_bytes = newline.unwrap_or(available.len());
        let next_len = line
            .len()
            .checked_add(content_bytes)
            .context("relay request frame length overflow")?;
        if next_len > maximum_bytes {
            bail!("relay request frame is too large");
        }
        line.extend_from_slice(&available[..content_bytes]);
        let consumed = content_bytes + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            return String::from_utf8(line)
                .context("relay request is not UTF-8")
                .map(Some);
        }
    }
}

/// A durable write can only fail permanently because the worker root is
/// gone: session teardown removed it under this daemon. Nothing served
/// afterwards could ever be persisted, so the daemon has to stop.
pub(super) fn worker_root_was_removed(body: &RelayResponseBody, root: &std::path::Path) -> bool {
    matches!(
        body,
        RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::Internal,
                ..
            }
        }
    ) && !root.is_dir()
}

/// Answer one relay request, keeping the relay lock for exactly as long as
/// the request needs it.
///
/// Every request but attach is a bounded mutation of durable state and is
/// served under the lock. Attach is not: validating a controller's cursor
/// can decompress a sealed segment and the reply carries up to a
/// [`hel::hel_worker::RELAY_REPLAY_BYTE_BUDGET`] page read from disk, and
/// a controller catching up over a long offline history asks for page
/// after page. Holding the lock across that stalls the coordinator's
/// `record_runtime_event`, and once its bounded event channel fills the
/// agent's turn stalls with it. So attach captures a plan under the lock
/// and does its reading on a blocking thread with the lock released.
pub(super) async fn handle_request(
    relay: &Arc<Mutex<DurableRelay>>,
    envelope: RelayRequestEnvelope,
) -> Result<RelayResponseEnvelope> {
    // Sealing and garbage collection can move the segments a plan named
    // while it is being read. That is not the controller's fault and not
    // its problem: plan again against the journal as it now stands. The
    // journal only moves that way once per sealed megabyte or per
    // acknowledgement, so a couple of attempts always outrun it.
    const REPLAN_ATTEMPTS: usize = 3;

    let mut deferred = {
        let mut guard = relay.lock().expect("relay state lock poisoned");
        match guard.take_deferred_attach(&envelope) {
            Some(deferred) => deferred,
            None => return Ok(guard.handle(envelope)),
        }
    };
    for _ in 0..REPLAN_ATTEMPTS {
        let generation = deferred.journal_generation();
        let response = tokio::task::spawn_blocking(move || deferred.finish())
            .await
            .context("assemble relay replay page")?;
        if !matches!(response.body, RelayResponseBody::Error { .. }) {
            let mut guard = relay.lock().expect("relay state lock poisoned");
            if guard.journal_generation() == generation {
                guard.remember_replay_cursor(&response);
            }
            return Ok(response);
        }
        let replanned = {
            let guard = relay.lock().expect("relay state lock poisoned");
            if guard.journal_generation() == generation {
                // The journal never moved, so this really is the answer.
                return Ok(response);
            }
            guard.take_deferred_attach(&envelope)
        };
        match replanned {
            Some(next) => deferred = next,
            None => break,
        }
    }
    Ok(DeferredRelayAttach::stale_journal_response(
        envelope.request_id,
        envelope.protocol_version,
    ))
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
            read_bounded_line(&mut reader, hel::hel_worker::MAX_FRAME_BYTES).await?
        {
            // One decoder owns this boundary: an unknown method is a
            // protocol error the controller can act on, and an unreadable
            // frame is answered rather than dropping the connection.
            let envelope = match decode_relay_request(line.as_bytes()) {
                DecodedRelayRequest::Known(envelope) => envelope,
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
            ) {
                let operation = envelope.request.method_name();
                let response = project_memory_response(envelope, &project_memory).await;
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if let RelayRequest::Reviewer { .. } = &envelope.request {
                // The reviewer is a sidecar with its own relay and its own
                // harness process. Both live on this connection's worker, so
                // the primary's relay never sees these.
                let operation = envelope.request.method_name();
                let response = reviewer_response(envelope, reviewer.as_ref()).await;
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if let RelayRequest::RespondElicitation { .. } = &envelope.request {
                // Form answers can contain private user input. They travel
                // directly to the ACP runtime and never touch relay state.
                let response = elicitation_response(envelope, commands.as_ref()).await;
                write_logged_response(&mut writer, &response, &session_id, "respond_elicitation")
                    .await?;
                continue;
            }
            let wakes_dispatch = matches!(&envelope.request, RelayRequest::Submit { .. });
            let checkpoint_change = checkpoint_change(&envelope.request);
            let operation = envelope.request.method_name();
            let response = match handle_request(&relay, envelope).await {
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
            if wakes_dispatch && accepted {
                wake_dispatch(&relay, &dispatch_wake)?;
            }
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

/// Serves the review dispatch socket for as long as the returned guard lives.
///
/// One line in, one line out, one connection per call: the supervisor's MCP
/// server connects, hands over the lanes it wants, and reads the answer. The
/// socket lives inside the worker root, so nothing outside this container can
/// reach it, and it is removed when the worker stops.
pub(super) fn serve_review_dispatch(
    root: &std::path::Path,
    reviewer: Arc<ReviewerSidecar>,
) -> Result<SocketGuard> {
    let directory = root.join(crate::hel_worker_runtime::REVIEWER_DIR);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create the reviewer directory {}", directory.display()))?;
    let path = directory.join(hel::hel_review::mcp::REVIEW_DISPATCH_SOCKET);
    // A socket left behind by a previous worker would refuse the bind; the
    // previous worker is gone, so its socket is stale by definition.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind the review dispatch socket {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let reviewer = reviewer.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_one_review_dispatch(stream, reviewer).await {
                    // Reported rather than dropped: a supervisor whose
                    // dispatch was lost would wait for lanes that never run.
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "a review dispatch could not be answered"
                    );
                }
            });
        }
    });
    Ok(SocketGuard(path))
}

async fn serve_one_review_dispatch(
    stream: UnixStream,
    reviewer: Arc<ReviewerSidecar>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let (read, mut write) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("read a review dispatch")?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let reply = match serde_json::from_str::<hel::hel_review::lanes::LaneDispatch>(line.trim()) {
        Ok(dispatch) => reviewer.record_dispatch(dispatch),
        Err(error) => hel::hel_review::lanes::LaneDispatchReply {
            started: Vec::new(),
            error: Some(format!("could not read the dispatch: {error}")),
        },
    };
    let mut body = serde_json::to_vec(&reply)?;
    body.push(b'\n');
    write
        .write_all(&body)
        .await
        .context("answer a review dispatch")?;
    write.flush().await.context("flush a review dispatch")
}

async fn reviewer_response(
    envelope: RelayRequestEnvelope,
    reviewer: Option<&Arc<ReviewerSidecar>>,
) -> RelayResponseEnvelope {
    if !envelope.request.supported_at(envelope.protocol_version) {
        return incompatible_request_protocol_response(
            envelope.request_id,
            envelope.protocol_version,
        );
    }
    let Some(reviewer) = reviewer else {
        return RelayResponseEnvelope {
            request_id: envelope.request_id,
            protocol_version: envelope.protocol_version,
            body: compaction_error(
                RelayErrorCode::InvalidState,
                "session is closed; no reviewer can run beside it",
            ),
        };
    };
    let RelayRequest::Reviewer { role, request } = envelope.request.clone() else {
        unreachable!("reviewer_response only serves reviewer requests");
    };
    // Operations on one role are sequential by construction; operations on
    // different roles are not, so a lane launching cannot block the controller
    // reading the supervisor's journal.
    reviewer.handle(envelope, role, request).await
}

fn compaction_error(code: RelayErrorCode, message: &str) -> RelayResponseBody {
    RelayResponseBody::Error {
        error: RelayProtocolError {
            code,
            message: message.to_owned(),
            retryable: false,
            detail: None,
        },
    }
}

async fn elicitation_response(
    envelope: RelayRequestEnvelope,
    commands: Option<&mpsc::Sender<CommandRequest>>,
) -> RelayResponseEnvelope {
    if !envelope.request.supported_at(envelope.protocol_version) {
        return incompatible_request_protocol_response(
            envelope.request_id,
            envelope.protocol_version,
        );
    }
    let protocol_version = envelope.protocol_version;
    let request_id = envelope.request_id;
    let RelayRequest::RespondElicitation {
        elicitation_id,
        response,
    } = envelope.request
    else {
        unreachable!("elicitation_response only serves form answers")
    };
    let body = match commands {
        None => compaction_error(
            RelayErrorCode::InvalidState,
            "session is closed; no ACP runtime can answer the elicitation",
        ),
        Some(commands) => {
            let (resolved, resolution) = tokio::sync::oneshot::channel();
            match commands
                .send(CommandRequest::ResolveElicitation {
                    elicitation_id: elicitation_id.clone(),
                    response,
                    resolved,
                })
                .await
            {
                Ok(()) => match resolution.await {
                    Ok(Ok(())) => RelayResponseBody::Ok {
                        payload: RelayResponsePayload::ElicitationResolved { elicitation_id },
                    },
                    Ok(Err(message)) => compaction_error(RelayErrorCode::InvalidState, &message),
                    Err(_) => compaction_error(
                        RelayErrorCode::Internal,
                        "ACP runtime stopped before resolving the elicitation",
                    ),
                },
                Err(_) => compaction_error(
                    RelayErrorCode::Internal,
                    "ACP runtime stopped before accepting the elicitation answer",
                ),
            }
        }
    };
    RelayResponseEnvelope {
        request_id,
        protocol_version,
        body,
    }
}

/// Serve a credential or skills request against this relay's own harness
/// home. File work runs on a blocking thread so neither the socket task
/// nor the ACP coordinator is stalled by filesystem I/O.
async fn credential_response(
    envelope: RelayRequestEnvelope,
    credentials: &std::result::Result<CredentialEndpoint, String>,
    relay_root: &std::path::Path,
) -> RelayResponseEnvelope {
    if !envelope.request.supported_at(envelope.protocol_version) {
        return incompatible_request_protocol_response(
            envelope.request_id,
            envelope.protocol_version,
        );
    }
    let body = match credentials {
        Err(message) => RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                message: message.clone(),
                retryable: false,
                detail: None,
            },
        },
        Ok(endpoint) => {
            let endpoint = endpoint.clone();
            let github_token_path = relay_root.join("github-token");
            let request = envelope.request.clone();
            match tokio::task::spawn_blocking(move || {
                apply_credential_request_at(&endpoint, &github_token_path, &request)
            })
            .await
            {
                Ok(Ok(payload)) => RelayResponseBody::Ok { payload },
                Ok(Err(error)) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::InvalidRequest,
                        message: format!("{error:#}"),
                        retryable: false,
                        detail: None,
                    },
                },
                Err(error) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::Internal,
                        message: format!("credential task stopped: {error}"),
                        retryable: true,
                        detail: None,
                    },
                },
            }
        }
    };
    RelayResponseEnvelope {
        request_id: envelope.request_id,
        protocol_version: envelope.protocol_version,
        body,
    }
}

/// Snapshot and install project memory off the socket task. These payloads
/// are connection-only and are never journaled as conversation history.
pub(super) async fn run_serialized_project_memory_io<T, F>(
    project_memory_io: &Arc<tokio::sync::Semaphore>,
    operation: F,
) -> std::result::Result<Result<T>, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    // A timed-out client can abandon this future, but Tokio cannot cancel
    // blocking filesystem work after it starts. Keep the permit inside
    // that work so reconnects wait instead of piling more reads and fsyncs
    // onto the degraded storage device.
    let permit = project_memory_io
        .clone()
        .acquire_owned()
        .await
        .expect("project memory I/O semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
}

async fn project_memory_response(
    envelope: RelayRequestEnvelope,
    endpoint: &ProjectMemoryEndpoint,
) -> RelayResponseEnvelope {
    if !envelope.request.supported_at(envelope.protocol_version) {
        return incompatible_request_protocol_response(
            envelope.request_id,
            envelope.protocol_version,
        );
    }
    let body = match endpoint.config.as_ref() {
        None => RelayResponseBody::Error {
            error: RelayProtocolError {
                code: RelayErrorCode::InvalidState,
                message: "this session has no project memory endpoint".into(),
                retryable: false,
                detail: None,
            },
        },
        Some(memory) => {
            let memory = memory.clone();
            let request = envelope.request.clone();
            match run_serialized_project_memory_io(&endpoint.io, move || {
                apply_project_memory_request(&memory, &request)
            })
            .await
            {
                Ok(Ok(payload)) => RelayResponseBody::Ok { payload },
                Ok(Err(error)) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::InvalidRequest,
                        message: format!("{error:#}"),
                        retryable: false,
                        detail: None,
                    },
                },
                Err(error) => RelayResponseBody::Error {
                    error: RelayProtocolError {
                        code: RelayErrorCode::Internal,
                        message: format!("project memory task stopped: {error}"),
                        retryable: true,
                        detail: None,
                    },
                },
            }
        }
    };
    RelayResponseEnvelope {
        request_id: envelope.request_id,
        protocol_version: envelope.protocol_version,
        body,
    }
}

pub(super) fn apply_project_memory_request(
    memory: &super::ProjectMemoryLaunchConfig,
    request: &RelayRequest,
) -> Result<RelayResponsePayload> {
    let replica = hel::hel_project_memory::ProjectMemoryStore::new(&memory.root);
    let baseline = hel::hel_project_memory::ProjectMemoryStore::new(&memory.baseline_root);
    match request {
        RelayRequest::ProjectMemorySnapshot => Ok(RelayResponsePayload::ProjectMemorySnapshot {
            baseline: baseline.snapshot()?,
            replica: replica.snapshot()?,
        }),
        RelayRequest::InstallProjectMemorySnapshot { snapshot } => {
            replica.install_snapshot(snapshot)?;
            baseline.install_snapshot(snapshot)?;
            Ok(RelayResponsePayload::ProjectMemorySnapshotInstalled)
        }
        other => bail!("{} is not a project memory request", other.method_name()),
    }
}

#[cfg(test)]
pub(super) fn apply_credential_request(
    endpoint: &CredentialEndpoint,
    request: &RelayRequest,
) -> Result<RelayResponsePayload> {
    apply_credential_request_at(endpoint, &endpoint.home.join("github-token"), request)
}

fn apply_credential_request_at(
    endpoint: &CredentialEndpoint,
    github_token_path: &std::path::Path,
    request: &RelayRequest,
) -> Result<RelayResponsePayload> {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use hel::hel_credentials::{
        CredentialSnapshot, MAX_CREDENTIAL_BYTES, MAX_GITHUB_TOKEN_BYTES, read_credential_file,
        read_github_token, remove_github_token, write_credential_file, write_github_token,
    };
    use hel::hel_skills::{
        MAX_SKILLS_ARCHIVE_BYTES, SkillsArchive, collect_skills, install_skills,
    };

    match request {
        RelayRequest::CredentialState => {
            let (snapshot, _) = read_credential_file(endpoint.harness, &endpoint.marker)?;
            Ok(credential_state_payload(&snapshot))
        }
        RelayRequest::ReadCredentials => {
            let (snapshot, bytes) = read_credential_file(endpoint.harness, &endpoint.marker)?;
            if !snapshot.present {
                bail!("session has no {} credentials", endpoint.marker.display());
            }
            Ok(RelayResponsePayload::Credentials {
                data: BASE64.encode(&bytes),
            })
        }
        RelayRequest::InstallCredentials { data } => {
            if data.len() > MAX_CREDENTIAL_BYTES * 2 {
                bail!("credential payload is above the {MAX_CREDENTIAL_BYTES} byte limit");
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode credential payload")?;
            write_credential_file(endpoint.harness, &endpoint.marker, &bytes)?;
            Ok(credential_state_payload(&CredentialSnapshot::of(
                endpoint.harness,
                &bytes,
            )))
        }
        RelayRequest::SkillsState => {
            let archive = collect_skills(endpoint.harness, &endpoint.home)?;
            Ok(skills_state_payload(&archive.state()))
        }
        RelayRequest::InstallSkills { data } => {
            // Base64 inflates by a third; rejecting early keeps a hostile
            // controller from making the worker buffer an endless frame.
            if data.len() > MAX_SKILLS_ARCHIVE_BYTES * 2 {
                bail!("skills payload is above the {MAX_SKILLS_ARCHIVE_BYTES} byte archive limit");
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode skills payload")?;
            let archive = SkillsArchive::decode(&bytes)?;
            install_skills(endpoint.harness, &endpoint.home, &archive)?;
            let installed = collect_skills(endpoint.harness, &endpoint.home)?;
            Ok(skills_state_payload(&installed.state()))
        }
        RelayRequest::GithubTokenState => {
            let (snapshot, _) = read_github_token(github_token_path)?;
            Ok(github_token_state_payload(&snapshot))
        }
        RelayRequest::InstallGithubToken { data } => {
            if data.len() > MAX_GITHUB_TOKEN_BYTES * 2 {
                bail!("GitHub token is above the {MAX_GITHUB_TOKEN_BYTES} byte limit");
            }
            let bytes = BASE64
                .decode(data.as_bytes())
                .context("decode GitHub token payload")?;
            let snapshot = write_github_token(github_token_path, &bytes)?;
            Ok(github_token_state_payload(&snapshot))
        }
        RelayRequest::RemoveGithubToken => {
            remove_github_token(github_token_path)?;
            Ok(github_token_state_payload(
                &hel::hel_credentials::GithubTokenSnapshot::absent(),
            ))
        }
        other => bail!(
            "{} is not a credential, GitHub token, or skills request",
            other.method_name()
        ),
    }
}

fn skills_state_payload(state: &hel::hel_skills::SkillsSyncState) -> RelayResponsePayload {
    RelayResponsePayload::SkillsState {
        present: state.present,
        fingerprint: state.fingerprint.clone(),
    }
}

fn credential_state_payload(
    snapshot: &hel::hel_credentials::CredentialSnapshot,
) -> RelayResponsePayload {
    RelayResponsePayload::CredentialState {
        present: snapshot.present,
        fingerprint: snapshot.fingerprint.clone(),
        freshness_epoch_ms: snapshot.freshness_epoch_ms,
    }
}

fn github_token_state_payload(
    snapshot: &hel::hel_credentials::GithubTokenSnapshot,
) -> RelayResponsePayload {
    RelayResponsePayload::GithubTokenState {
        present: snapshot.present,
        fingerprint: snapshot.fingerprint.clone(),
    }
}

pub(super) fn configure_github_cli(
    root: &std::path::Path,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const ORIGINAL_BASH_ENV: &str = "MJ_ORIGINAL_BASH_ENV";

    let bin = root.join("bin");
    if std::fs::symlink_metadata(&bin).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "GitHub CLI wrapper directory {} is a symbolic link",
            bin.display()
        );
    }
    std::fs::create_dir_all(&bin)
        .with_context(|| format!("create GitHub CLI wrapper directory {}", bin.display()))?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700))?;

    let wrapper = bin.join("gh");
    if std::fs::symlink_metadata(&wrapper).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "GitHub CLI wrapper {} is a symbolic link",
            wrapper.display()
        );
    }
    const WRAPPER: &str = r#"#!/bin/sh
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
clean_path=
old_ifs=$IFS
IFS=:
for entry in $PATH; do
    if [ "$entry" != "$script_dir" ]; then
        if [ -z "$clean_path" ]; then clean_path=$entry; else clean_path=$clean_path:$entry; fi
    fi
done
IFS=$old_ifs
PATH=$clean_path
export PATH
token_file=$script_dir/../github-token
if [ -f "$token_file" ]; then
    IFS= read -r GH_TOKEN < "$token_file"
    export GH_TOKEN
    unset GITHUB_TOKEN
else
    unset GH_TOKEN GITHUB_TOKEN
fi
exec gh "$@"
"#;
    hel::hel_config::atomic_write_existing(&wrapper, WRAPPER.as_bytes())?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;

    // Harnesses can start `bash -lc`, whose login profile may replace PATH
    // after the ACP bridge inherited it. BASH_ENV is read after that profile.
    let shell_environment = root.join("github-shell-env");
    if std::fs::symlink_metadata(&shell_environment)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!(
            "GitHub CLI shell environment {} is a symbolic link",
            shell_environment.display()
        );
    }
    const SHELL_ENVIRONMENT: &str = r#"if [ -n "${MJ_ORIGINAL_BASH_ENV:-}" ] && [ "${MJ_ORIGINAL_BASH_ENV}" != "${BASH_ENV:-}" ] && [ -r "${MJ_ORIGINAL_BASH_ENV}" ]; then
    . "${MJ_ORIGINAL_BASH_ENV}"
fi
if [ -n "${MJ_GITHUB_CLI_BIN:-}" ]; then
    case ":${PATH:-}:" in
        *:"${MJ_GITHUB_CLI_BIN}":*) ;;
        *) PATH="${MJ_GITHUB_CLI_BIN}${PATH:+:${PATH}}"; export PATH ;;
    esac
fi
"#;
    hel::hel_config::atomic_write_existing(&shell_environment, SHELL_ENVIRONMENT.as_bytes())?;
    std::fs::set_permissions(&shell_environment, std::fs::Permissions::from_mode(0o600))?;

    let inherited_path = environment
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let bin_text = bin.to_string_lossy().into_owned();
    environment.insert(
        "PATH".into(),
        if std::env::split_paths(std::ffi::OsStr::new(&inherited_path))
            .any(|entry| entry.as_path() == bin.as_path())
        {
            inherited_path
        } else if inherited_path.is_empty() {
            bin_text.clone()
        } else {
            format!("{bin_text}:{inherited_path}")
        },
    );
    environment.insert(super::GITHUB_CLI_BIN_ENV.into(), bin_text);
    let shell_environment_text = shell_environment.to_string_lossy().into_owned();
    let original_bash_env = environment
        .get("BASH_ENV")
        .filter(|configured| configured.as_str() != shell_environment_text)
        .cloned()
        .or_else(|| environment.get(ORIGINAL_BASH_ENV).cloned());
    match original_bash_env {
        Some(original) => {
            environment.insert(ORIGINAL_BASH_ENV.into(), original);
        }
        None => {
            environment.remove(ORIGINAL_BASH_ENV);
        }
    }
    environment.insert("BASH_ENV".into(), shell_environment_text);
    configure_github_git_helpers(environment, &wrapper)?;

    let inherited_token = std::env::var("GH_TOKEN")
        .ok()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok());
    if let Some(token) = inherited_token
        && hel::hel_credentials::validate_github_token(token.as_bytes()).is_ok()
    {
        hel::hel_credentials::write_github_token(&root.join("github-token"), token.as_bytes())?;
    }
    Ok(())
}

fn configure_github_git_helpers(
    environment: &mut BTreeMap<String, String>,
    wrapper: &std::path::Path,
) -> Result<()> {
    // Environment config stays scoped to the harness process tree. The empty
    // value clears image/user helpers before the absolute Hel helper is added.
    let helper = format!(
        "!{} auth git-credential",
        hel::hel_targets::posix_quote(&wrapper.to_string_lossy())
    );
    for host in ["github.com", "gist.github.com"] {
        let key = format!("credential.https://{host}.helper");
        append_git_config(environment, &key, "")?;
        append_git_config(environment, &key, &helper)?;
    }
    Ok(())
}

fn append_git_config(
    environment: &mut BTreeMap<String, String>,
    key: &str,
    value: &str,
) -> Result<()> {
    let count = environment
        .get("GIT_CONFIG_COUNT")
        .map(|count| {
            count
                .parse::<usize>()
                .with_context(|| format!("invalid GIT_CONFIG_COUNT {count:?}"))
        })
        .transpose()?
        .unwrap_or(0);
    if (0..count).any(|index| {
        environment
            .get(&format!("GIT_CONFIG_KEY_{index}"))
            .map(String::as_str)
            == Some(key)
            && environment
                .get(&format!("GIT_CONFIG_VALUE_{index}"))
                .map(String::as_str)
                == Some(value)
    }) {
        return Ok(());
    }
    environment.insert(format!("GIT_CONFIG_KEY_{count}"), key.to_owned());
    environment.insert(format!("GIT_CONFIG_VALUE_{count}"), value.to_owned());
    environment.insert(
        "GIT_CONFIG_COUNT".into(),
        count
            .checked_add(1)
            .context("too many inherited Git configuration entries")?
            .to_string(),
    );
    Ok(())
}

enum CheckpointChange {
    Begin(String),
    /// The barrier no longer belongs to this connection, whether it ended
    /// through a full completion or an early dispatch release.
    Ended(String),
}

fn checkpoint_change(request: &RelayRequest) -> Option<CheckpointChange> {
    let RelayRequest::Submit {
        command_id,
        command,
    } = request
    else {
        return None;
    };
    match command {
        RelayCommand::BeginCheckpoint { .. } => Some(CheckpointChange::Begin(command_id.clone())),
        RelayCommand::CompleteCheckpoint { barrier_command_id }
        | RelayCommand::ReleaseCheckpoint { barrier_command_id } => {
            Some(CheckpointChange::Ended(barrier_command_id.clone()))
        }
        _ => None,
    }
}

fn report_fatal(
    fatal: &mpsc::Sender<anyhow::Error>,
    error: anyhow::Error,
    session_id: &str,
    reason: &str,
) {
    let detail = format!("{error:#}");
    tracing::error!(
        %session_id,
        %reason,
        error = %detail,
        "relay daemon reported a fatal failure"
    );
    if let Err(send_error) = fatal.try_send(error) {
        tracing::error!(
            %session_id,
            %reason,
            error = %send_error,
            "could not deliver relay fatal failure to daemon"
        );
    }
}

fn release_checkpoint_barriers(
    relay: &Arc<Mutex<DurableRelay>>,
    dispatch_wake: &mpsc::Sender<()>,
    checkpoint_barriers: BTreeSet<String>,
) -> Result<()> {
    let mut released = false;
    {
        let mut relay = relay.lock().expect("relay state lock poisoned");
        for command_id in checkpoint_barriers {
            released |= relay
                .cancel_checkpoint_barrier_on_disconnect(&command_id)
                .with_context(|| format!("release disconnected checkpoint barrier {command_id}"))?
                .is_some();
        }
    }
    if released {
        wake_dispatch(relay, dispatch_wake)
            .context("wake relay after releasing checkpoint barrier")?;
    }
    Ok(())
}

pub(super) fn wake_dispatch(
    relay: &Arc<Mutex<DurableRelay>>,
    dispatch_wake: &mpsc::Sender<()>,
) -> Result<()> {
    match dispatch_wake.try_send(()) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(()),
        Err(mpsc::error::TrySendError::Closed(()))
            if relay
                .lock()
                .expect("relay state lock poisoned")
                .operational_state()
                .execution
                == hel::hel_worker::RelayExecutionState::Closed =>
        {
            Ok(())
        }
        Err(mpsc::error::TrySendError::Closed(())) => bail!("relay coordinator stopped"),
    }
}

pub(super) async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &RelayResponseEnvelope,
) -> Result<()> {
    let mut encoded = serde_json::to_vec(response)?;
    if encoded.len().saturating_add(1) > hel::hel_worker::MAX_FRAME_BYTES {
        bail!("relay response frame is too large");
    }
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

/// Protocol rejections are ordinary responses rather than Rust errors, so
/// the socket loop must log them explicitly. Keep this next to the write
/// boundary so every request route (including credentials and old
/// protocol methods) gets the same session and operation context.
async fn write_logged_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &RelayResponseEnvelope,
    session_id: &str,
    operation: &str,
) -> Result<()> {
    if let RelayResponseBody::Error { error } = &response.body {
        tracing::warn!(
            %session_id,
            %operation,
            request_id = %response.request_id,
            protocol_version = response.protocol_version,
            relay_error_code = ?error.code,
            relay_retryable = error.retryable,
            error_message = %error.message,
            "relay request returned an error"
        );
    }
    write_response(writer, response).await
}

pub(super) async fn forward_proxy_streams(
    mut client_read: impl tokio::io::AsyncRead + Unpin,
    mut client_write: impl tokio::io::AsyncWrite + Unpin,
    mut relay_read: impl tokio::io::AsyncRead + Unpin,
    mut relay_write: impl tokio::io::AsyncWrite + Unpin,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The proxy must die with its client. Joining both copy directions
    // left the process alive forever after stdin EOF (a killed `podman
    // exec` client), leaking one thread-heavy process per poll inside the
    // container. Exit as soon as either side closes. An idle connection
    // is intentional: it may own a checkpoint barrier while the
    // controller transfers a large archive. Before the first request,
    // however, a bounded deadline prevents a detached Podman conmon from
    // holding the target-side proxy forever after the controller kills a
    // launcher whose handshake timed out.
    let mut client_buf = [0_u8; 64 * 1024];
    let mut relay_buf = [0_u8; 64 * 1024];
    let first_count = match tokio::time::timeout(
        PROXY_INITIAL_INPUT_TIMEOUT,
        client_read.read(&mut client_buf),
    )
    .await
    {
        Ok(read) => read.context("read initial proxy stdin")?,
        Err(_) => {
            tracing::debug!(
                operation = "proxy_initial_input",
                "relay proxy client sent no initial frame before the idle deadline"
            );
            return Ok(());
        }
    };
    if first_count == 0 {
        if let Err(error) = relay_write.shutdown().await {
            tracing::debug!(
                operation = "proxy_shutdown",
                %error,
                "could not close relay socket after proxy client EOF"
            );
        }
        return Ok(());
    }
    relay_write
        .write_all(&client_buf[..first_count])
        .await
        .context("forward initial request to worker")?;

    loop {
        tokio::select! {
            read = client_read.read(&mut client_buf) => {
                let count = read.context("read proxy stdin")?;
                if count == 0 {
                    // Client is gone; flush any final in-flight response
                    // briefly, then exit.
                    if let Err(error) = relay_write.shutdown().await {
                        tracing::debug!(
                            operation = "proxy_shutdown",
                            %error,
                            "could not close relay socket after proxy client EOF"
                        );
                    }
                    if let Err(error) = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        tokio::io::copy(&mut relay_read, &mut client_write),
                    )
                    .await
                    {
                        tracing::debug!(
                            operation = "proxy_final_response",
                            %error,
                            "could not forward the final relay response before proxy shutdown"
                        );
                    }
                    return Ok(());
                }
                relay_write
                    .write_all(&client_buf[..count])
                    .await
                    .context("forward request to worker")?;
            }
            read = relay_read.read(&mut relay_buf) => {
                let count = read.context("read worker socket")?;
                if count == 0 {
                    return Ok(());
                }
                client_write
                    .write_all(&relay_buf[..count])
                    .await
                    .context("forward response to client")?;
                client_write.flush().await.context("flush proxy stdout")?;
            }
        }
    }
}

pub async fn proxy(root: PathBuf) -> Result<()> {
    let stream = UnixStream::connect(root.join("control.sock"))
        .await
        .with_context(|| format!("connect worker at {}", root.display()))?;
    let (socket_read, socket_write) = stream.into_split();
    let outcome = forward_proxy_streams(
        tokio::io::stdin(),
        tokio::io::stdout(),
        socket_read,
        socket_write,
    )
    .await;

    // Leave now instead of returning into runtime shutdown. Tokio's global
    // stdin reader holds a blocking pool thread in a read that cannot be
    // cancelled, and a client that is waiting for a response keeps the
    // proxy's stdin open, so dropping the runtime would wait forever. A
    // proxy whose worker socket is gone has nothing left to carry: the
    // client must see this process exit and its stdout close, not a live
    // proxy with no worker behind it.
    let mut stdout = tokio::io::stdout();
    if let Err(error) = stdout.flush().await {
        tracing::debug!(
            operation = "proxy_flush",
            %error,
            "could not flush proxy stdout before exit"
        );
    }
    match outcome {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            // The client reads this proxy's stderr into its own log.
            tracing::error!(
                operation = "proxy",
                error = format!("{error:#}"),
                "relay proxy stopped with an error"
            );
            std::process::exit(1);
        }
    }
}

/// Own the ACP bridge's process group.  The daemon communicates only with
/// this supervisor; if the daemon is killed, stdin reaches EOF and the
/// complete bridge process tree is terminated and reaped.
pub async fn run_acp_supervisor(spec: AcpSupervisorSpec) -> Result<()> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // This hidden subcommand exclusively owns its stdio pipes. Tokio's global
    // stdin reader uses a non-cancellable blocking read, which can keep the
    // supervisor process alive after its ACP child has already been reaped.
    // SAFETY: ownership of each process fd is transferred exactly once here.
    let stdin = unsafe { OwnedFd::from_raw_fd(libc::STDIN_FILENO) };
    // SAFETY: stdout is likewise owned exclusively by this subcommand.
    let stdout = unsafe { OwnedFd::from_raw_fd(libc::STDOUT_FILENO) };
    let stdin = tokio::net::unix::pipe::Receiver::from_owned_fd(stdin)
        .context("open ACP supervisor stdin pipe")?;
    let stdout = tokio::net::unix::pipe::Sender::from_owned_fd(stdout)
        .context("open ACP supervisor stdout pipe")?;
    run_acp_supervisor_with_streams(spec, stdin, stdout).await
}

pub(super) async fn run_acp_supervisor_with_streams<R, W>(
    spec: AcpSupervisorSpec,
    mut parent_stdin: R,
    mut parent_stdout: W,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let _harness_lease = spec
        .harness_lease
        .as_deref()
        .map(super::harness::acquire_supervisor_lease)
        .transpose()?;
    let mut command = tokio::process::Command::new(&spec.command);
    command
        .args(&spec.args)
        .envs(&spec.environment)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .current_dir(&spec.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("launch supervised ACP bridge {}", spec.command.display()))?;
    let pid = child
        .id()
        .context("supervised ACP bridge has no process ID")? as i32;
    let mut child_stdin = child.stdin.take().context("ACP bridge stdin unavailable")?;
    let mut child_stdout = child
        .stdout
        .take()
        .context("ACP bridge stdout unavailable")?;
    enum SupervisorCompletion {
        InputEnded,
        OutputEnded,
        ChildExited(std::process::ExitStatus),
    }

    let completion = {
        let input = tokio::io::copy(&mut parent_stdin, &mut child_stdin);
        let output = tokio::io::copy(&mut child_stdout, &mut parent_stdout);
        let exited = child.wait();
        tokio::pin!(input, output, exited);
        tokio::select! {
            result = &mut input => {
                result.context("forward ACP supervisor input")?;
                SupervisorCompletion::InputEnded
            }
            result = &mut output => {
                result.context("forward ACP supervisor output")?;
                SupervisorCompletion::OutputEnded
            }
            result = &mut exited => {
                SupervisorCompletion::ChildExited(
                    result.context("wait for supervised ACP bridge")?
                )
            }
        }
    };
    if !matches!(&completion, SupervisorCompletion::ChildExited(_)) {
        match tokio::time::timeout(std::time::Duration::from_secs(1), child_stdin.shutdown()).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::debug!(
                    operation = "acp_supervisor_shutdown",
                    %error,
                    "could not close ACP bridge stdin before termination"
                );
            }
            Err(_) => {
                tracing::warn!(
                    operation = "acp_supervisor_shutdown",
                    "timed out closing ACP bridge stdin before termination"
                );
            }
        }
    }
    drop(child_stdin);
    terminate_process_group(pid, libc::SIGTERM);
    let (bridge_ended, status) = match completion {
        SupervisorCompletion::ChildExited(status) => (true, Some(status)),
        SupervisorCompletion::InputEnded => {
            match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                Ok(status) => {
                    let status = status.context("wait for supervised ACP bridge")?;
                    (false, Some(status))
                }
                Err(_) => {
                    terminate_process_group(pid, libc::SIGKILL);
                    child.wait().await.context("reap supervised ACP bridge")?;
                    (false, None)
                }
            }
        }
        SupervisorCompletion::OutputEnded => {
            match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                Ok(status) => {
                    let status = status.context("wait for supervised ACP bridge")?;
                    (true, Some(status))
                }
                Err(_) => {
                    terminate_process_group(pid, libc::SIGKILL);
                    child.wait().await.context("reap supervised ACP bridge")?;
                    (true, None)
                }
            }
        }
    };
    if bridge_ended
        && let Some(status) = status
        && !status.success()
    {
        bail!("supervised ACP bridge exited with {status}");
    }
    Ok(())
}

/// Make this process lead its own session, so session teardown can stop
/// the whole worker tree with a single process-group signal. Failing means
/// the process already leads its group, which is the state we wanted.
///
/// Only the real daemon entry point may call this: it detaches the caller
/// from its controlling terminal.
pub fn lead_process_group() {
    // SAFETY: setsid takes no arguments and changes only this process's
    // own session and process-group membership.
    unsafe {
        libc::setsid();
    }
}
