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
use tokio::sync::mpsc;

use super::reviewer::{ReviewerCancellation, ReviewerPlacement, ReviewerSidecar};
use super::{AcpSupervisorSpec, CredentialEndpoint, WorkerLaunchConfig};

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

pub(super) struct SocketGuard(pub(super) PathBuf);

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
    let mut target_environment = config.target_environment.clone();
    target_environment.extend(config.environment);
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
    // The first capture must have a point before the primary harness starts:
    // users may enter a session with dirty or untracked files already in the
    // checkout, and those files are not this turn's work. A relay restart is
    // different: replacing a missing baseline then could hide changes from a
    // review that was interrupted. A restored relay has no state file yet, so
    // its restored worktree is a safe fresh-session boundary.
    // Only a session a review can run for pays for a baseline. For every other
    // session, and for every sub-agent child, startup runs no Git command at
    // all: the capture exists solely to tell a later review what the turn
    // changed.
    if !relay_state_exists && !checkpoint_only && config.review_capture {
        super::record_startup_step(&root, "review-baseline");
        let mut workspace_roots = vec![config.cwd.clone()];
        workspace_roots.extend(config.additional_directories.iter().cloned());
        tokio::task::spawn_blocking(move || {
            let git = mj_checkpoint::archive::SystemGit;
            let repositories =
                crate::review::capture::discover_repositories(&git, &workspace_roots);
            crate::review::capture::initialize_review_baselines(&git, &repositories)
        })
        .await
        .map_err(|error| anyhow::anyhow!("review baseline initialization stopped: {error}"))??;
    }
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
    let project_memory = ProjectMemoryEndpoint::new(config.project_memory.clone());
    if !checkpoint_only && resume_session.is_none()
        // Recreating an unused native thread keeps this relay's original
        // startup context, which may already belong to a pending prompt.
        && durable_relay.operational_state().native_session_id.is_none()
        && config.harness != HarnessKind::Claude
        && let Some(memory) = &config.project_memory
    {
        let store = mj_core::project_memory::ProjectMemoryStore::new(&memory.root);
        durable_relay.install_prompt_context(mj_core::project_memory::startup_prompt_context(
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
    super::record_startup_step(&root, "bind-socket");
    let listener = bind_unix_listener(&socket)
        .with_context(|| format!("bind worker socket {}", socket.display()))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set worker socket {} nonblocking", socket.display()))?;
    let listener = UnixListener::from_std(listener)
        .with_context(|| format!("register worker socket {}", socket.display()))?;
    let _socket_guard = SocketGuard(socket.clone());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    super::record_startup_step(&root, "serving");

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

    let base_environment = mj_core::login_environment::resolve().await?;
    let mut session_environment = base_environment.clone();
    session_environment.extend(config.environment.clone());
    configure_github_cli(&root, &mut session_environment)?;
    // Persist only explicit and Mjolnir-generated overrides, never shell exports.
    config.environment = session_environment
        .iter()
        .filter(|(name, value)| {
            config.environment.contains_key(*name) || base_environment.get(*name) != Some(*value)
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
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
        session_environment.extend(managed.environment.clone());
    }
    let harness_gc = managed_harness
        .as_ref()
        .map(|managed| super::harness::spawn_gc(managed.cache_root.clone(), config.harness));

    let (acp_commands_tx, acp_commands_rx) = mpsc::channel(32);
    let (acp_events_tx, acp_events_rx) = mpsc::channel(ACP_EVENT_CHANNEL_CAPACITY);
    let (dispatch_wake_tx, dispatch_wake_rx) = mpsc::channel(1);
    let user_shells = crate::user_shell::UserShellRegistry::new(
        config.cwd.clone(),
        session_environment.clone(),
        acp_events_tx.clone(),
    );
    // The bridge environment has to be final before it is persisted. The
    // accepted selectors are not part of it: the ACP runtime re-pins them into
    // this spec before every bridge start, including the first one.
    let accepted_config = {
        let relay = relay.lock().expect("relay lock poisoned");
        let state = relay.operational_state();
        acp::AcceptedSessionConfig::from_configuration(&state.config, &state.config_options)
    };
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
        target_environment: config.target_environment.clone(),
        worker_root: root.clone(),
        session_id: config.session_id.clone(),
        cwd: config.cwd.clone(),
        additional_directories: config.additional_directories.clone(),
        worker_executable: worker_executable.clone(),
        harness_runtime: config.harness_runtime,
        review_capture: config.review_capture,
    }));
    // The review supervisor's dispatch tool talks to this worker over its own
    // socket inside the reviewer directory: an MCP server started by a harness
    // has no relay connection, and the dispatch is not session history.
    let dispatch_socket = serve_review_dispatch(&root, reviewer.clone())?;
    let (subagents, _subagent_socket_guard) = if config.subagent_tools {
        let (endpoint, guard) = super::subagents::serve(&root)?;
        (Some(endpoint), Some(guard))
    } else {
        (None, None)
    };
    // Everything that can start a reviewer runs inside this block, so every
    // way out of it — including an error — passes through the pause below.
    // Stopping the reviewer's process group before this worker exits is what
    // keeps a harness from outliving the session it was reviewing for.
    // One acquisition: a guard taken inside the struct literal below would
    // live until the literal ends and deadlock the next one.
    let (acp_activity, step_clock, tools_in_flight, accepted_config) = {
        let relay = relay.lock().expect("relay lock poisoned");
        (
            relay.acp_activity_clock(),
            relay.step_clock(),
            relay.tools_in_flight(),
            Arc::new(Mutex::new(accepted_config)),
        )
    };
    let outcome = async {
        if let Some(request) = &config.goal_resume_request {
            let mut state = relay.lock().expect("relay lock poisoned");
            if state.operational_state().goal.answered_resume.as_ref() != Some(request) {
                state.record_session_update(serde_json::from_value(serde_json::json!({"sessionUpdate":"session_info_update", "_meta":{"mjGoalResumePending":request}}))?)?;
            }
        }
        let goal_recovery = Arc::new(Mutex::new(mj_core::goal::GoalRecoveryContext {
            state: relay.lock().expect("relay lock poisoned").operational_state().goal,
            request: config.goal_resume_request.clone(),
            journal: Some(mj_core::goal::GoalJournal({
                let relay = relay.clone();
                Arc::new(move |update| {
                    relay.lock().expect("relay lock poisoned").record_session_update(update)?;
                    Ok(())
                })
            })),
        }));
        let acp_spec = LaunchSpec {
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
            additional_directories: config.additional_directories,
            extra_mcp_servers: Vec::new(),
            subagent_mcp_socket: subagents
                .as_ref()
                .map(|_| root.join(super::subagents::SUBAGENT_SOCKET)),
            project_memory: config.project_memory,
            resume_session,
            native_session_may_have_history,
            accepted_config,
            harness: config.harness,
            execution_policy: config.execution_policy,
            acp_activity,
            step_clock,
            tools_in_flight,
            stall_policy: None,
        };
        let mut acp_task = tokio::spawn(acp::run(acp_spec, acp_commands_rx, acp_events_tx));

        let event_relay = relay.clone();
        let mut event_task = tokio::spawn(run_relay_coordinator_with_shells(
            event_relay,
            acp_events_rx,
            dispatch_wake_rx,
            acp_commands_tx.clone(),
            user_shells,
            kimi_task_home.map(KimiTaskMonitor::new),
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
                    let client_subagents = subagents.clone();
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
                                subagents: client_subagents,
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
            == mj_core::relay::RelayExecutionState::Closed;
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
    let mut environment = config.target_environment.clone();
    environment.extend(config.environment);
    config.environment = environment;
    let prepared = super::harness::resolve(
        config.harness_runtime,
        config.harness,
        config.execution_policy,
        &config.environment,
    )
    .await
    .with_context(|| format!("prepare managed {}", config.harness.display_name()))?;
    if config.harness_runtime == mj_core::worker_launch::HarnessRuntimePolicy::Managed
        && prepared.is_none()
    {
        bail!("managed harness preparation produced no installation");
    }
    Ok(())
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
            if let Some(body) = mj_core::relay::protocol::relay_protocol_rejection(&envelope) {
                let response = RelayResponseEnvelope {
                    request_id: envelope.request_id,
                    protocol_version: envelope.protocol_version,
                    body,
                };
                write_logged_response(&mut writer, &response, &session_id, "incompatible").await?;
                continue;
            }
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
                let response = reviewer_response(envelope, reviewer.as_ref(), &mut reader).await;
                write_logged_response(&mut writer, &response, &session_id, operation).await?;
                continue;
            }
            if matches!(
                &envelope.request,
                RelayRequest::SubagentRequests | RelayRequest::CompleteSubagentRequest { .. }
            ) {
                let operation = envelope.request.method_name();
                let request_id = envelope.request_id.clone();
                let protocol_version = envelope.protocol_version;
                let body = match (&subagents, envelope.request) {
                    (Some(endpoint), RelayRequest::SubagentRequests) => {
                        let (requests, results) = endpoint.snapshot();
                        RelayResponseBody::Ok {
                            payload: RelayResponsePayload::SubagentRequests { requests, results },
                        }
                    }
                    (Some(endpoint), RelayRequest::CompleteSubagentRequest { result }) => {
                        match endpoint.complete(result) {
                            Ok(()) => RelayResponseBody::Ok {
                                payload: RelayResponsePayload::SubagentRequestCompleted,
                            },
                            Err(error) => compaction_error(
                                RelayErrorCode::Internal,
                                &format!("persist sub-agent result: {error:#}"),
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
                    _ => unreachable!(),
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
                let response = elicitation_response(envelope, commands.as_ref()).await;
                write_logged_response(&mut writer, &response, &session_id, "respond_elicitation")
                    .await?;
                continue;
            }
            if let RelayRequest::StopBackgroundTask { .. } = &envelope.request {
                let response =
                    background_task_stop_response(envelope, commands.as_ref(), &relay).await;
                write_logged_response(&mut writer, &response, &session_id, "stop_background_task")
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
