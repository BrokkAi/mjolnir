//! ACP client runtime: spawns the agent subprocess, wires JSON-RPC over
//! stdio, and bridges UI commands/events through two mpsc channels.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, CancelNotification, ClientCapabilities,
    CloseSessionRequest, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    CreateTerminalRequest, CreateTerminalResponse, ElicitationAcceptAction, ElicitationAction,
    ElicitationCapabilities, ElicitationFormCapabilities, ElicitationUrlCapabilities, ErrorCode,
    FileSystemCapabilities, ForkSessionRequest, ImageContent, Implementation, InitializeRequest,
    KillTerminalRequest, KillTerminalResponse, LoadSessionRequest, NewSessionRequest,
    PermissionOption, PermissionOptionKind, PromptRequest, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOption, SessionConfigValueId, SessionId,
    SessionInfoUpdate, SessionModeState, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, TerminalExitStatus, TerminalId,
    TerminalOutputRequest, TerminalOutputResponse, TextContent, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo};
use anyhow::Result;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::event::{
    ElicitationOutcome, ElicitationPrompt, LoadSessionResult, PermissionDecision, PermissionPrompt,
    PromptImage, SessionConfigTarget, TerminalOutputSnapshot, UiCommand, UiEvent,
};
use crate::install;
use crate::paths::{WorkspaceRoots, normalize_spawn_program, path_is_under_any_root};

pub struct AcpRuntimeConfig {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Additional absolute workspace roots to pass to ACP session lifecycle
    /// requests. These expand workspace scope but do not imply trust.
    pub additional_directories: Vec<PathBuf>,
    pub resume_session: Option<String>,
    /// Environment variables to inject into the spawned agent process.
    /// Used for agents that require knobs like `AUGMENT_DISABLE_AUTO_UPDATE=1`.
    pub env: HashMap<String, String>,
    /// Where the agent's stderr should go. `None` discards it (via
    /// `Stdio::null()`, which maps to /dev/null on Unix and NUL on
    /// Windows) so the agent's logs don't bleed into the TUI. Pass a
    /// path to capture for debugging.
    pub agent_stderr: Option<PathBuf>,
    /// Maximum text bytes returned by ACP filesystem reads or accepted by
    /// ACP filesystem writes.
    pub fs_max_text_bytes: u64,
}

#[derive(Clone)]
struct RuntimeSessionState {
    active_session_id: Arc<Mutex<Option<SessionId>>>,
    active_roots: Arc<Mutex<Vec<PathBuf>>>,
    cancelled_permission_sessions: Arc<Mutex<HashSet<SessionId>>>,
    permission_cancel_generation: watch::Sender<u64>,
}

#[derive(Clone)]
struct ConnectedEventFields {
    agent_name: Option<String>,
    agent_version: Option<String>,
    prompt_images_supported: bool,
    session_fork_supported: bool,
}

impl RuntimeSessionState {
    fn new() -> Self {
        let (permission_cancel_generation, _) = watch::channel(0);
        Self {
            active_session_id: Arc::new(Mutex::new(None)),
            active_roots: Arc::new(Mutex::new(Vec::new())),
            cancelled_permission_sessions: Arc::new(Mutex::new(HashSet::new())),
            permission_cancel_generation,
        }
    }

    async fn is_active_session(&self, session_id: &SessionId) -> bool {
        self.active_session_id.lock().await.as_ref() == Some(session_id)
    }

    #[cfg(test)]
    async fn set_active_session(
        &self,
        session_id: SessionId,
        fs_root: &Path,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        self.set_active_session_with_roots(session_id, fs_root, &[])
            .await
    }

    async fn set_active_session_with_roots(
        &self,
        session_id: SessionId,
        fs_root: &Path,
        additional_roots: &[PathBuf],
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        let roots = WorkspaceRoots::new(fs_root, additional_roots)
            .map_err(|e| {
                agent_client_protocol::Error::invalid_params()
                    .data(serde_json::Value::String(e.to_string()))
            })?
            .active_roots();
        *self.active_session_id.lock().await = Some(session_id);
        *self.active_roots.lock().await = roots;
        Ok(())
    }

    async fn clear_active_session(&self) {
        *self.active_session_id.lock().await = None;
        self.active_roots.lock().await.clear();
    }

    async fn ensure_active_session(
        &self,
        session_id: &SessionId,
        capability: &str,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if self.is_active_session(session_id).await {
            return Ok(());
        }
        Err(
            agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(
                format!("{capability} request for inactive session"),
            )),
        )
    }

    async fn active_root_set(
        &self,
        session_id: &SessionId,
        capability: &str,
    ) -> std::result::Result<Vec<PathBuf>, agent_client_protocol::Error> {
        self.ensure_active_session(session_id, capability).await?;
        let roots = self.active_roots.lock().await.clone();
        if roots.is_empty() {
            Err(
                agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(
                    format!("{capability} root is not active"),
                )),
            )
        } else {
            Ok(roots)
        }
    }

    async fn permission_cancelled(&self, session_id: &SessionId) -> bool {
        self.cancelled_permission_sessions
            .lock()
            .await
            .contains(session_id)
    }

    async fn mark_permissions_cancelled(&self, session_id: &SessionId) {
        self.cancelled_permission_sessions
            .lock()
            .await
            .insert(session_id.clone());
        let next = self.permission_cancel_generation.borrow().wrapping_add(1);
        let _ = self.permission_cancel_generation.send(next);
    }

    async fn clear_permissions_cancelled(&self, session_id: &SessionId) {
        self.cancelled_permission_sessions
            .lock()
            .await
            .remove(session_id);
    }

    fn subscribe_permission_cancellations(&self) -> watch::Receiver<u64> {
        self.permission_cancel_generation.subscribe()
    }

    async fn wait_until_permission_cancelled(
        &self,
        session_id: &SessionId,
        cancel_rx: &mut watch::Receiver<u64>,
    ) {
        loop {
            if self.permission_cancelled(session_id).await {
                return;
            }
            if cancel_rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// User-facing classification of launch-phase failures. Each variant
/// renders as a one-line headline plus an action hint on the next line;
/// `UiEvent::Fatal` carries that text through to the transcript so users
/// see a `command not found` differently from an `auth required`.
#[derive(Debug)]
pub enum LaunchError {
    /// `spawn` returned ENOENT for the agent command.
    CommandNotFound { command: String },
    /// `spawn` failed for some other reason (permissions, OS limits, ...).
    SpawnFailed {
        command: String,
        source: std::io::Error,
    },
    /// Opening the `--agent-stderr` capture file failed. Distinct from
    /// `SpawnFailed` because the remediation is "fix the --agent-stderr
    /// flag", not "fix the --command flag".
    StderrFileOpen {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    /// The ACP `initialize` handshake errored or the agent never replied
    /// to it. Often a wrong protocol version or a crashed agent.
    InitializeFailed {
        source: agent_client_protocol::Error,
    },
    /// The agent returned `auth_required` (-32000) during initialize or
    /// session lifecycle setup. The agent is healthy; the user just needs
    /// to authenticate first.
    AuthRequired { detail: Option<String> },
    /// The agent negotiated an ACP protocol version this client does not support.
    UnsupportedProtocolVersion { negotiated: ProtocolVersion },
    /// The user requested a lifecycle method the agent did not advertise.
    UnsupportedCapability { capability: &'static str },
    /// `session/new` failed for some other reason (bad cwd, agent-side
    /// crash, ...).
    SessionCreateFailed {
        source: agent_client_protocol::Error,
    },
    /// uvx was requested but uv could not be installed automatically.
    UvInstallFailed { source: String },
    /// npx was requested but embedded Node could not be installed automatically.
    NodeInstallFailed { source: String },
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::CommandNotFound { command } => write!(
                f,
                "agent command not found: {command}\n\
                 hint: install the agent on PATH or pass --command </path/to/agent>"
            ),
            LaunchError::SpawnFailed { command, source } => write!(
                f,
                "could not spawn agent {command}: {source}\n\
                 hint: check executable permissions and that --command is right"
            ),
            LaunchError::StderrFileOpen { path, source } => write!(
                f,
                "could not open agent stderr file {}: {source}\n\
                 hint: check --agent-stderr <path> is writable and its parent directory exists",
                path.display()
            ),
            LaunchError::InitializeFailed { source } => write!(
                f,
                "agent did not complete the ACP initialize handshake: {source}\n\
                 hint: confirm the agent speaks ACP v1; capture --agent-stderr for detail"
            ),
            LaunchError::AuthRequired { detail } => {
                let detail = detail.as_deref().unwrap_or("no detail provided");
                write!(
                    f,
                    "agent requires authentication before opening a session: {detail}\n\
                     hint: see the agent's docs to authenticate, then relaunch mj"
                )
            }
            LaunchError::UnsupportedProtocolVersion { negotiated } => write!(
                f,
                "agent negotiated unsupported ACP protocol version {negotiated}\n\
                 hint: update mjolnir or choose an agent that supports ACP {}",
                ProtocolVersion::LATEST
            ),
            LaunchError::UnsupportedCapability { capability } => write!(
                f,
                "agent does not advertise ACP capability {capability}\n\
                 hint: choose an agent that supports {capability}, or avoid the command that requires it"
            ),
            LaunchError::SessionCreateFailed { source } => write!(
                f,
                "agent rejected session/new: {source}\n\
                 hint: verify --cwd is accessible to the agent"
            ),
            LaunchError::UvInstallFailed { source } => write!(
                f,
                "uvx is required for this agent, but mj could not install uv automatically: {source}\n\
                 hint: install uv from https://docs.astral.sh/uv/getting-started/installation/ and relaunch mj"
            ),
            LaunchError::NodeInstallFailed { source } => write!(
                f,
                "npx is required for this agent, but mj could not install embedded Node 24 automatically: {source}\n\
                 hint: install Node.js 24 from https://nodejs.org/en/download and relaunch mj"
            ),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Send `UiEvent::Fatal` and mark it as sent so the tail of `run` does
/// not emit a generic follow-up Fatal for the same failure.
fn emit_fatal(
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    fatal_emitted: &Arc<AtomicBool>,
    msg: String,
) {
    if !fatal_emitted.swap(true, Ordering::SeqCst) {
        let _ = ui_tx.send(UiEvent::Fatal(msg));
    }
}

/// Classify a spawn-time `io::Error`. `ErrorKind::NotFound` becomes
/// `CommandNotFound`; everything else falls through to `SpawnFailed`.
fn classify_spawn_error(command: &std::path::Path, source: std::io::Error) -> LaunchError {
    let command = command.display().to_string();
    if source.kind() == std::io::ErrorKind::NotFound {
        LaunchError::CommandNotFound { command }
    } else {
        LaunchError::SpawnFailed { command, source }
    }
}

/// Extract an `AuthRequired` detail from an ACP error if the code matches.
/// Returns `Some(detail)` for any auth-required error (regardless of the
/// stage that produced it) and `None` otherwise.
fn auth_required_detail(source: &agent_client_protocol::Error) -> Option<Option<String>> {
    if source.code != ErrorCode::AuthRequired {
        return None;
    }
    let detail = source.data.as_ref().map(|d| match d {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    Some(detail)
}

/// Classify an ACP error from the `initialize` handshake. Auth-required
/// is split out so users get the same actionable text as on session/new;
/// the spec permits an agent to demand auth before opening any session.
fn classify_initialize_error(source: agent_client_protocol::Error) -> LaunchError {
    match auth_required_detail(&source) {
        Some(detail) => LaunchError::AuthRequired { detail },
        None => LaunchError::InitializeFailed { source },
    }
}

/// Classify a session lifecycle ACP error. Auth-required is split out
/// because it has a different remediation than a generic failure.
fn classify_session_error(source: agent_client_protocol::Error) -> LaunchError {
    match auth_required_detail(&source) {
        Some(detail) => LaunchError::AuthRequired { detail },
        None => LaunchError::SessionCreateFailed { source },
    }
}

fn validate_protocol_version(negotiated: ProtocolVersion) -> std::result::Result<(), LaunchError> {
    if negotiated == ProtocolVersion::LATEST {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedProtocolVersion { negotiated })
    }
}

fn require_load_session(capabilities: &AgentCapabilities) -> std::result::Result<(), LaunchError> {
    if capabilities.load_session {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedCapability {
            capability: "loadSession",
        })
    }
}

fn require_resume_or_load_session(
    capabilities: &AgentCapabilities,
) -> std::result::Result<(), LaunchError> {
    if capabilities.session_capabilities.resume.is_some() || capabilities.load_session {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedCapability {
            capability: "sessionCapabilities.resume or loadSession",
        })
    }
}

fn require_additional_directories(
    capabilities: &AgentCapabilities,
    additional_directories: &[PathBuf],
) -> std::result::Result<(), LaunchError> {
    if additional_directories.is_empty()
        || capabilities
            .session_capabilities
            .additional_directories
            .is_some()
    {
        Ok(())
    } else {
        Err(LaunchError::UnsupportedCapability {
            capability: "sessionCapabilities.additionalDirectories",
        })
    }
}

fn new_session_request(cwd: PathBuf, additional_directories: &[PathBuf]) -> NewSessionRequest {
    NewSessionRequest::new(cwd).additional_directories(additional_directories.to_vec())
}

fn resume_session_request(
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
) -> ResumeSessionRequest {
    ResumeSessionRequest::new(session_id, cwd)
        .additional_directories(additional_directories.to_vec())
}

fn load_session_request(
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
) -> LoadSessionRequest {
    LoadSessionRequest::new(session_id, cwd).additional_directories(additional_directories.to_vec())
}

fn fork_session_request(
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
) -> ForkSessionRequest {
    ForkSessionRequest::new(session_id, cwd).additional_directories(additional_directories.to_vec())
}

async fn resume_existing_session(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    capabilities: &AgentCapabilities,
    auth_methods: &[AuthMethod],
) -> std::result::Result<Option<(Vec<SessionConfigOption>, Vec<SessionConfigTarget>)>, LaunchError>
{
    if capabilities.session_capabilities.resume.is_some() {
        let resume_req = resume_session_request(session_id, cwd, additional_directories);
        let resumed = match conn.send_request(resume_req.clone()).block_task().await {
            Ok(s) => s,
            Err(source) => match auth_required_detail(&source) {
                Some(detail) => {
                    authenticate_after_auth_required(conn, auth_methods, detail).await?;
                    conn.send_request(resume_req)
                        .block_task()
                        .await
                        .map_err(classify_session_error)?
                }
                None => return Err(classify_session_error(source)),
            },
        };
        return Ok(session_config_from_parts(
            resumed.config_options,
            resumed.modes,
        ));
    }

    require_load_session(capabilities)?;
    let load_req = load_session_request(session_id, cwd, additional_directories);
    let loaded = match conn.send_request(load_req.clone()).block_task().await {
        Ok(s) => s,
        Err(source) => match auth_required_detail(&source) {
            Some(detail) => {
                authenticate_after_auth_required(conn, auth_methods, detail).await?;
                conn.send_request(load_req)
                    .block_task()
                    .await
                    .map_err(classify_session_error)?
            }
            None => return Err(classify_session_error(source)),
        },
    };
    Ok(session_config_from_parts(
        loaded.config_options,
        loaded.modes,
    ))
}

async fn authenticate_after_auth_required(
    conn: &ConnectionTo<Agent>,
    auth_methods: &[AuthMethod],
    detail: Option<String>,
) -> std::result::Result<(), LaunchError> {
    let Some(method) = auth_methods.first() else {
        return Err(LaunchError::AuthRequired { detail });
    };

    conn.send_request(AuthenticateRequest::new(method.id().clone()))
        .block_task()
        .await
        .map(|_| ())
        .map_err(classify_session_error)
}

/// User-facing message for an agent process that exited without us
/// asking. Shared between the `child.wait()` race in `run` (which
/// catches the exit as it happens) and the post-drive `try_wait()`
/// snapshot (which catches it after `drive_client` returned an Err).
/// Both produce identical wording so users see one consistent
/// explanation regardless of which path detected it.
fn agent_exited_unexpectedly_msg(detail: impl std::fmt::Display) -> String {
    format!(
        "agent process exited unexpectedly: {detail}\n\
         hint: capture --agent-stderr to see the agent's last output"
    )
}

/// Spawn the agent subprocess and run the ACP client to completion.
/// Pumps `ui_rx` for `UiCommand`s and emits `UiEvent`s onto `ui_tx`.
///
/// Returns once the connection is closed or the user requests shutdown.
pub async fn run(
    cfg: AcpRuntimeConfig,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
) -> Result<()> {
    let fatal_emitted = Arc::new(AtomicBool::new(false));

    let prepared = match prepare_agent_command_for_spawn(&cfg.command, &cfg.env, &ui_tx).await {
        Ok(prepared) => prepared,
        Err(launch_err) => {
            let text = launch_err.to_string();
            emit_fatal(&ui_tx, &fatal_emitted, text.clone());
            return Err(anyhow::anyhow!(text));
        }
    };

    let (mut child, child_stdin, child_stdout) = match spawn_agent(
        &prepared.command,
        &cfg.args,
        &prepared.env,
        cfg.agent_stderr.as_deref(),
        SpawnIsolation::ProcessGroup,
    ) {
        Ok(spawned) => spawned,
        Err(launch_err) => {
            let text = launch_err.to_string();
            emit_fatal(&ui_tx, &fatal_emitted, text.clone());
            return Err(anyhow::anyhow!(text));
        }
    };
    // Snapshot the agent PID up front. It doubles as the process-group
    // id (Unix) / Windows process-group root, so we can still target
    // the entire descendant tree later even if `child.wait()` or
    // `try_wait()` has already reaped the immediate child by the time
    // we call `kill_agent_tree`.
    let agent_pid = child.id();
    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

    // Race the ACP client against `child.wait()`. If the agent process
    // dies on its own (crash, panic, exit-without-shutdown), the JSON-RPC
    // transport closes silently and otherwise just looks like a series of
    // failed prompts. Catching the exit here surfaces a single, clear
    // Fatal instead of an unbounded stream of "prompt failed" warnings.
    //
    // `biased;` with `drive_result` first: when the user quits cleanly
    // (drive_result = Ok) and the agent also happens to exit in the same
    // poll (because it noticed EOF on stdin), we want the clean-shutdown
    // outcome, not a spurious "agent exited unexpectedly" Fatal. The wait
    // branch only wins when drive is still pending.
    let result: Result<()> = {
        let drive = drive_client_with_fs_limit(
            transport,
            cfg.cwd.clone(),
            cfg.additional_directories.clone(),
            cfg.resume_session.clone(),
            ui_tx.clone(),
            ui_rx,
            fatal_emitted.clone(),
            cfg.fs_max_text_bytes,
        );
        tokio::pin!(drive);
        tokio::select! {
            biased;
            drive_result = &mut drive => drive_result,
            wait_result = child.wait() => {
                let detail = match wait_result {
                    Ok(status) => format!("exit status {status}"),
                    Err(e) => format!("wait failed: {e}"),
                };
                let msg = agent_exited_unexpectedly_msg(detail);
                emit_fatal(&ui_tx, &fatal_emitted, msg.clone());
                Err(anyhow::anyhow!(msg))
            }
        }
    };

    // Snapshot whether the child died on its own *before* we touch it,
    // so the post-drive Fatal can distinguish "agent crashed" from
    // "we killed it after a different error".
    let pre_kill_exit = child.try_wait().ok().flatten();

    // Reap the entire agent subtree, not just the immediate child.
    // Wrappers like `uvx brokk acp` fork a Python interpreter as a
    // grandchild; killing only the wrapper PID orphans the grandchild
    // and leaks the actual agent across mjolnir sessions.
    kill_agent_tree(&mut child, agent_pid).await;
    // Generic catch-all: anything that escaped the launch-phase classifier
    // (e.g. a transport error after initialize succeeded) gets a plain
    // fatal so the user sees *something*. Launch-phase failures and the
    // child-wait branch above will already have called `emit_fatal` with
    // action text, and the guard suppresses a second emission.
    if let Err(e) = &result {
        // Race-condition handling: drive_client can return with a raw
        // `Broken pipe` before the `child.wait()` arm fires, leaving the
        // user with no action text. If the child *had* already exited at
        // that point, swap in the friendly "agent exited" wording.
        let msg = if let Some(status) = pre_kill_exit {
            agent_exited_unexpectedly_msg(format!("exit status {status}"))
        } else {
            format!("acp: {e}")
        };
        emit_fatal(&ui_tx, &fatal_emitted, msg);
    }
    result
}

pub(crate) struct PreparedAgentCommand {
    pub(crate) command: PathBuf,
    pub(crate) env: HashMap<String, String>,
}

pub(crate) async fn prepare_agent_command_for_spawn(
    command: &Path,
    env: &HashMap<String, String>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    let prepared = prepare_agent_command(command, ui_tx).await?;
    let mut merged_env = prepared.env;
    merged_env.extend(env.clone());
    Ok(PreparedAgentCommand {
        command: prepared.command,
        env: merged_env,
    })
}

async fn prepare_agent_command(
    command: &Path,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    let command = normalize_spawn_program(command.to_path_buf());
    if is_program_name(&command, "uvx") {
        return prepare_uvx_command(command, ui_tx).await;
    }
    if is_program_name(&command, "npx") {
        return prepare_npx_command(command, ui_tx).await;
    }
    Ok(PreparedAgentCommand {
        command,
        env: HashMap::new(),
    })
}

/// Resolve an agent launch command for spawning **without triggering any
/// install**. Used by the startup validation probe, which must never kick
/// off a multi-hundred-megabyte uv/Node/binary download just to check
/// whether an agent is reachable.
///
/// Returns `None` when the launcher (`uvx`/`npx`) or the program itself is
/// not already present, so the caller can mark the agent "not installed"
/// rather than installing it. Mirrors the env-merging order of
/// [`prepare_agent_command_for_spawn`]: launcher-provided env first, then
/// the agent's own env on top.
pub(crate) fn resolve_agent_command_no_install(
    command: &Path,
    env: &HashMap<String, String>,
) -> Option<PreparedAgentCommand> {
    let command = normalize_spawn_program(command.to_path_buf());
    let (resolved, mut merged_env) = if is_program_name(&command, "uvx") {
        let path = find_on_path(&command).or_else(|| {
            let embedded = embedded_uvx_path();
            is_executable_file(&embedded).then_some(embedded)
        })?;
        (path, embedded_uv_env())
    } else if is_program_name(&command, "npx") {
        let path = find_on_path(&command)
            .or_else(|| embedded_npx_path().filter(|p| is_executable_file(p)))?;
        (path, HashMap::new())
    } else {
        // Plain program or explicit path: must already resolve on PATH or
        // exist on disk.
        (find_on_path(&command)?, HashMap::new())
    };
    merged_env.extend(env.clone());
    Some(PreparedAgentCommand {
        command: resolved,
        env: merged_env,
    })
}

async fn prepare_uvx_command(
    command: PathBuf,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    if let Some(path) = find_on_path(&command) {
        return Ok(PreparedAgentCommand {
            command: path,
            env: embedded_uv_env(),
        });
    }

    let _ = ui_tx.send(UiEvent::Info(
        "uvx not found; installing uv for uvx-based agents".to_string(),
    ));
    install_uv().await?;
    let uvx_path = embedded_uvx_path();
    if is_executable_file(&uvx_path) {
        let _ = ui_tx.send(UiEvent::Info("uv installed; launching agent".to_string()));
        return Ok(PreparedAgentCommand {
            command: uvx_path,
            env: embedded_uv_env(),
        });
    }
    Err(LaunchError::UvInstallFailed {
        source: format!(
            "installer completed but uvx was not found at {}",
            embedded_uvx_path().display()
        ),
    })
}

async fn prepare_npx_command(
    command: PathBuf,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<PreparedAgentCommand, LaunchError> {
    if let Some(path) = find_on_path(&command) {
        return Ok(PreparedAgentCommand {
            command: path,
            env: HashMap::new(),
        });
    }

    let _ = ui_tx.send(UiEvent::Info(
        "npx not found; installing embedded Node 24 for npx-based agents".to_string(),
    ));
    install_node24().await?;
    let Some(npx_path) = embedded_npx_path() else {
        return Err(LaunchError::NodeInstallFailed {
            source: format!(
                "installer completed but npx was not found under {}",
                embedded_node_root().display()
            ),
        });
    };
    let _ = ui_tx.send(UiEvent::Info(
        "embedded Node 24 installed; launching agent".to_string(),
    ));
    Ok(PreparedAgentCommand {
        command: npx_path,
        env: embedded_node_env(),
    })
}

fn is_program_name(command: &Path, expected: &str) -> bool {
    command.components().count() == 1 && command.file_stem().is_some_and(|name| name == expected)
}

fn find_on_path(command: &Path) -> Option<PathBuf> {
    if command.components().count() != 1 {
        return command.exists().then(|| command.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(command);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let extensions = std::env::var_os("PATHEXT")
                .map(|v| {
                    v.to_string_lossy()
                        .split(';')
                        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    ["com", "exe", "bat", "cmd"]
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                });
            for ext in extensions {
                let mut with_ext = candidate.clone();
                with_ext.set_extension(ext);
                if is_executable_file(&with_ext) {
                    return Some(with_ext);
                }
            }
        }
        None
    })
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn embedded_uv_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mj")
        .join("runners")
        .join("uv")
}

fn embedded_uv_bin_dir() -> PathBuf {
    embedded_uv_root().join("bin")
}

fn embedded_uvx_path() -> PathBuf {
    #[cfg(windows)]
    {
        embedded_uv_bin_dir().join("uvx.exe")
    }
    #[cfg(not(windows))]
    {
        embedded_uv_bin_dir().join("uvx")
    }
}

fn embedded_uv_env() -> HashMap<String, String> {
    let root = embedded_uv_root();
    HashMap::from([
        (
            "UV_CACHE_DIR".to_string(),
            root.join("cache").display().to_string(),
        ),
        (
            "UV_TOOL_DIR".to_string(),
            root.join("tools").display().to_string(),
        ),
        (
            "UV_TOOL_BIN_DIR".to_string(),
            root.join("tool-bin").display().to_string(),
        ),
        (
            "UV_PYTHON_INSTALL_DIR".to_string(),
            root.join("python").display().to_string(),
        ),
        (
            "UV_PYTHON_BIN_DIR".to_string(),
            root.join("python-bin").display().to_string(),
        ),
    ])
}

async fn install_uv() -> std::result::Result<(), LaunchError> {
    let bin_dir = embedded_uv_bin_dir();
    tokio::fs::create_dir_all(&bin_dir)
        .await
        .map_err(|e| LaunchError::UvInstallFailed {
            source: format!("failed to create {}: {e}", bin_dir.display()),
        })?;
    let mut cmd = uv_install_command(&bin_dir);
    let output = tokio::time::timeout(Duration::from_secs(180), cmd.output())
        .await
        .map_err(|_| LaunchError::UvInstallFailed {
            source: "installer timed out after 180 seconds".to_string(),
        })?
        .map_err(|e| LaunchError::UvInstallFailed {
            source: format!("failed to start installer: {e}"),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(LaunchError::UvInstallFailed {
        source: command_failure_summary(&output),
    })
}

fn uv_install_command(bin_dir: &Path) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-ExecutionPolicy",
            "ByPass",
            "-Command",
            "irm https://astral.sh/uv/install.ps1 | iex",
        ]);
        cmd.env("UV_UNMANAGED_INSTALL", bin_dir);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "curl -LsSf https://astral.sh/uv/install.sh | sh"]);
        cmd.env("UV_UNMANAGED_INSTALL", bin_dir);
        cmd
    }
}

fn embedded_node_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mj")
        .join("runners")
        .join("node")
        .join("24")
}

#[cfg(windows)]
fn embedded_node_bin_dir() -> Option<PathBuf> {
    embedded_node_dir()
}

#[cfg(not(windows))]
fn embedded_node_bin_dir() -> Option<PathBuf> {
    embedded_node_dir().map(|dir| dir.join("bin"))
}

fn embedded_node_dir() -> Option<PathBuf> {
    let root = embedded_node_root();
    let entries = std::fs::read_dir(root).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir() && embedded_npx_path_in_dir(path).is_some())
}

fn embedded_npx_path() -> Option<PathBuf> {
    embedded_node_dir().and_then(|dir| embedded_npx_path_in_dir(&dir))
}

fn embedded_npx_path_in_dir(dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let path = dir.join("npx.cmd");
        is_executable_file(&path).then_some(path)
    }
    #[cfg(not(windows))]
    {
        let path = dir.join("bin").join("npx");
        is_executable_file(&path).then_some(path)
    }
}

fn embedded_node_env() -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(bin_dir) = embedded_node_bin_dir() {
        env.insert("PATH".to_string(), prepend_to_path(&bin_dir));
    }
    env
}

fn prepend_to_path(dir: &Path) -> String {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths)
        .unwrap_or_else(|_| dir.as_os_str().to_owned())
        .to_string_lossy()
        .into_owned()
}

async fn install_node24() -> std::result::Result<(), LaunchError> {
    let root = embedded_node_root();
    let sentinel = root.join(".installed");
    if sentinel.exists() && embedded_npx_path().is_some() {
        return Ok(());
    }
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("failed to create {}: {e}", root.display()),
        })?;
    let archive_url = node24_archive_url().await?;
    let (tx, _rx) = mpsc::unbounded_channel::<install::Progress>();
    install::download_and_extract(&archive_url, &root, &tx)
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: e.to_string(),
        })?;
    if embedded_npx_path().is_none() {
        return Err(LaunchError::NodeInstallFailed {
            source: format!("npx not found after extracting {archive_url}"),
        });
    }
    tokio::fs::write(&sentinel, archive_url)
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("failed to write {}: {e}", sentinel.display()),
        })?;
    Ok(())
}

async fn node24_archive_url() -> std::result::Result<String, LaunchError> {
    let suffix = node24_archive_suffix().ok_or_else(|| LaunchError::NodeInstallFailed {
        source: format!(
            "unsupported platform for embedded Node 24: {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    })?;
    let shasums_url = "https://nodejs.org/dist/latest-v24.x/SHASUMS256.txt";
    let body = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("build http client: {e}"),
        })?
        .get(shasums_url)
        .send()
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("GET {shasums_url}: {e}"),
        })?
        .error_for_status()
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("GET {shasums_url}: {e}"),
        })?
        .text()
        .await
        .map_err(|e| LaunchError::NodeInstallFailed {
            source: format!("read {shasums_url}: {e}"),
        })?;
    let file = body
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .find(|file| file.ends_with(suffix))
        .ok_or_else(|| LaunchError::NodeInstallFailed {
            source: format!("Node 24 archive matching {suffix} not listed in SHASUMS256.txt"),
        })?;
    Ok(format!("https://nodejs.org/dist/latest-v24.x/{file}"))
}

fn node24_archive_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x64.tar.gz"),
        ("linux", "aarch64") => Some("linux-arm64.tar.gz"),
        ("macos", "x86_64") => Some("darwin-x64.tar.gz"),
        ("macos", "aarch64") => Some("darwin-arm64.tar.gz"),
        ("windows", "x86_64") => Some("win-x64.zip"),
        ("windows", "aarch64") => Some("win-arm64.zip"),
        _ => None,
    }
}

pub(crate) fn client_implementation() -> Implementation {
    Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")).title("Mjolnir")
}

fn command_failure_summary(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = stderr
        .trim()
        .lines()
        .last()
        .or_else(|| stdout.trim().lines().last())
        .unwrap_or("no installer output");
    format!("installer exited with {}; {detail}", output.status)
}

/// Run the full ACP client state machine over an arbitrary transport with
/// default filesystem text limits. Factored out of `run` so integration tests
/// can plug in an in-process duplex stream and drive a mock agent without
/// spawning a subprocess.
#[cfg(test)]
pub async fn drive_client<T>(
    transport: T,
    cwd: PathBuf,
    resume_session: Option<String>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
) -> Result<()>
where
    T: ConnectTo<Client>,
{
    drive_client_with_fs_limit(
        transport,
        cwd,
        Vec::new(),
        resume_session,
        ui_tx,
        ui_rx,
        fatal_emitted,
        DEFAULT_FS_TEXT_BYTES,
    )
    .await
}

#[cfg(test)]
pub async fn drive_client_with_additional_directories<T>(
    transport: T,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    resume_session: Option<String>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
) -> Result<()>
where
    T: ConnectTo<Client>,
{
    drive_client_with_fs_limit(
        transport,
        cwd,
        additional_directories,
        resume_session,
        ui_tx,
        ui_rx,
        fatal_emitted,
        DEFAULT_FS_TEXT_BYTES,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive_client_with_fs_limit<T>(
    transport: T,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    resume_session: Option<String>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    mut ui_rx: mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
    fs_max_text_bytes: u64,
) -> Result<()>
where
    T: ConnectTo<Client>,
{
    // Channel for permission prompts that the UI needs to answer.
    // The on_receive_request closure forwards (req, responder) here and
    // returns immediately so the JSON-RPC dispatch loop stays unblocked.
    let session_state = RuntimeSessionState::new();
    let terminals = Arc::new(ManagedTerminals::with_session_state(
        ui_tx.clone(),
        session_state.clone(),
    ));
    let filesystem = Arc::new(LocalFileSystem::new(
        session_state.clone(),
        ui_tx.clone(),
        fs_max_text_bytes,
    ));
    let perm_ui_tx = ui_tx.clone();
    let elicit_ui_tx = ui_tx.clone();
    let perm_session_state = session_state.clone();
    let notif_ui_tx = ui_tx.clone();
    let notif_session_state = session_state.clone();
    let read_filesystem = filesystem.clone();
    let write_filesystem = filesystem.clone();
    let create_terminals = terminals.clone();
    let output_terminals = terminals.clone();
    let release_terminals = terminals.clone();
    let wait_terminals = terminals.clone();
    let kill_terminals = terminals.clone();
    let drive_terminals = terminals.clone();
    let result = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                if notif_session_state
                    .is_active_session(&notification.session_id)
                    .await
                {
                    let _ = notif_ui_tx.send(UiEvent::SessionUpdate(notification.update));
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let session_id = request.session_id.clone();
                if !perm_session_state.is_active_session(&session_id).await {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                if perm_session_state
                    .permission_cancelled(&session_id)
                    .await
                {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                let mut cancel_rx = perm_session_state.subscribe_permission_cancellations();
                let (tx, rx) = oneshot::channel::<PermissionDecision>();
                let prompt = PermissionPrompt {
                    tool_call: request.tool_call,
                    options: request.options,
                    responder: tx,
                };
                if perm_ui_tx.send(UiEvent::PermissionRequest(prompt)).is_err() {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                let outcome = tokio::select! {
                    decision = rx => {
                        match decision {
                            Ok(PermissionDecision::Selected(id)) => {
                                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                            }
                            _ => RequestPermissionOutcome::Cancelled,
                        }
                    }
                    () = perm_session_state.wait_until_permission_cancelled(&session_id, &mut cancel_rx) => {
                        let _ = perm_ui_tx.send(UiEvent::CancelPendingPermissions);
                        RequestPermissionOutcome::Cancelled
                    }
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateElicitationRequest, responder, cx| {
                // Unlike permissions, do NOT gate on `is_active_session`:
                // request-scoped elicitations (the `/setup` case) have no
                // session and would be wrongly dropped. Render whatever
                // arrives; the UI degrades unsupported shapes to `decline`.
                let (tx, rx) = oneshot::channel::<ElicitationOutcome>();
                let prompt = ElicitationPrompt {
                    message: request.message.clone(),
                    mode: request.mode.clone(),
                    responder: tx,
                };
                if elicit_ui_tx
                    .send(UiEvent::ElicitationRequest(prompt))
                    .is_err()
                {
                    return responder
                        .respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
                }
                // `Err(_)` means the UI tore down without answering (responder
                // dropped); treat it as Cancel, mirroring permission semantics.
                cx.spawn(async move {
                    let action = match rx.await {
                        Ok(ElicitationOutcome::Accept(content)) => {
                            ElicitationAction::Accept(
                                ElicitationAcceptAction::new().content(content),
                            )
                        }
                        Ok(ElicitationOutcome::Decline) => ElicitationAction::Decline,
                        Ok(ElicitationOutcome::Cancel) | Err(_) => ElicitationAction::Cancel,
                    };
                    responder.respond(CreateElicitationResponse::new(action))
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReadTextFileRequest, responder, _cx| {
                responder.respond_with_result(read_filesystem.read_text_file(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WriteTextFileRequest, responder, _cx| {
                responder.respond_with_result(write_filesystem.write_text_file(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateTerminalRequest, responder, _cx| {
                responder.respond_with_result(create_terminals.create(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: TerminalOutputRequest, responder, _cx| {
                responder.respond_with_result(output_terminals.output(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReleaseTerminalRequest, responder, _cx| {
                responder.respond_with_result(release_terminals.release(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WaitForTerminalExitRequest, responder, _cx| {
                responder.respond_with_result(wait_terminals.wait_for_exit(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: KillTerminalRequest, responder, _cx| {
                responder.respond_with_result(kill_terminals.kill(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
            if let Err(e) = drive_session(
                conn,
                cwd,
                additional_directories,
                resume_session,
                &ui_tx,
                &mut ui_rx,
                fatal_emitted,
                session_state,
                drive_terminals,
            )
            .await
            {
                let msg = format!("{e:#}");
                return Err(agent_client_protocol::Error::internal_error()
                    .data(serde_json::Value::String(msg)));
            }
            Ok(())
        })
        .await;

    terminals.shutdown_all().await;
    result.map_err(|e| anyhow::anyhow!("acp client error: {e}"))?;
    Ok(())
}

/// Initialize the agent, open a session, then loop forwarding prompts and
/// cancellations until the UI requests shutdown or the agent closes the
/// connection.
#[allow(clippy::too_many_arguments)]
async fn drive_session(
    conn: ConnectionTo<Agent>,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    resume_session: Option<String>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    fatal_emitted: Arc<AtomicBool>,
    session_state: RuntimeSessionState,
    terminals: Arc<ManagedTerminals>,
) -> Result<()> {
    // Advertise the client capabilities backed by handlers registered in
    // `drive_client` above.
    let init_req = InitializeRequest::new(ProtocolVersion::V1)
        .client_info(client_implementation())
        .client_capabilities(
            ClientCapabilities::new()
                .fs(FileSystemCapabilities::new()
                    .read_text_file(true)
                    .write_text_file(true))
                .terminal(true)
                // Advertise both elicitation modes. `form` covers single-select
                // `/setup` menus and `url` covers OAuth-login steps. v1 fully
                // implements single-select forms and URL/QR; any richer form
                // shape degrades to a `decline` response (a valid answer), so
                // advertising `form` does not over-promise.
                .elicitation(
                    ElicitationCapabilities::new()
                        .form(ElicitationFormCapabilities::new())
                        .url(ElicitationUrlCapabilities::new()),
                ),
        );
    let init_resp = match conn.send_request(init_req).block_task().await {
        Ok(r) => r,
        Err(source) => {
            let launch_err = classify_initialize_error(source);
            let text = launch_err.to_string();
            emit_fatal(ui_tx, &fatal_emitted, text.clone());
            return Err(anyhow::anyhow!(text));
        }
    };
    if let Err(launch_err) = validate_protocol_version(init_resp.protocol_version) {
        let text = launch_err.to_string();
        emit_fatal(ui_tx, &fatal_emitted, text.clone());
        return Err(anyhow::anyhow!(text));
    }
    if let Err(launch_err) =
        require_additional_directories(&init_resp.agent_capabilities, &additional_directories)
    {
        let text = launch_err.to_string();
        emit_fatal(ui_tx, &fatal_emitted, text.clone());
        return Err(anyhow::anyhow!(text));
    }
    let connected_fields = ConnectedEventFields {
        agent_name: init_resp.agent_info.as_ref().map(|i| i.name.clone()),
        agent_version: init_resp.agent_info.as_ref().map(|i| i.version.clone()),
        prompt_images_supported: init_resp.agent_capabilities.prompt_capabilities.image,
        // `session/fork` is exposed by the ACP crate as an unstable extension;
        // only surface the built-in command when the agent explicitly advertises it.
        session_fork_supported: init_resp
            .agent_capabilities
            .session_capabilities
            .fork
            .is_some(),
    };
    emit_connected(ui_tx, &connected_fields);

    let (mut session_id, initial_config, resumed) = match resume_session {
        Some(existing_session_id) => {
            let session_id = SessionId::from(existing_session_id.clone());
            let initial_config = match resume_existing_session(
                &conn,
                session_id.clone(),
                cwd.clone(),
                &additional_directories,
                &init_resp.agent_capabilities,
                &init_resp.auth_methods,
            )
            .await
            {
                Ok(initial_config) => initial_config,
                Err(launch_err) => {
                    let text = launch_err.to_string();
                    emit_fatal(ui_tx, &fatal_emitted, text.clone());
                    return Err(anyhow::anyhow!(text));
                }
            };
            session_state
                .set_active_session_with_roots(session_id.clone(), &cwd, &additional_directories)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            (session_id, initial_config, true)
        }
        None => match conn
            .send_request(new_session_request(cwd.clone(), &additional_directories))
            .block_task()
            .await
        {
            Ok(s) => {
                let config = session_config_from_parts(s.config_options, s.modes);
                session_state
                    .set_active_session_with_roots(
                        s.session_id.clone(),
                        &cwd,
                        &additional_directories,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                (s.session_id, config, false)
            }
            Err(source) => match auth_required_detail(&source) {
                Some(detail) => {
                    if let Err(launch_err) =
                        authenticate_after_auth_required(&conn, &init_resp.auth_methods, detail)
                            .await
                    {
                        let text = launch_err.to_string();
                        emit_fatal(ui_tx, &fatal_emitted, text.clone());
                        return Err(anyhow::anyhow!(text));
                    }
                    match conn
                        .send_request(new_session_request(cwd.clone(), &additional_directories))
                        .block_task()
                        .await
                    {
                        Ok(s) => {
                            let config = session_config_from_parts(s.config_options, s.modes);
                            session_state
                                .set_active_session_with_roots(
                                    s.session_id.clone(),
                                    &cwd,
                                    &additional_directories,
                                )
                                .await
                                .map_err(|e| anyhow::anyhow!("{e}"))?;
                            (s.session_id, config, false)
                        }
                        Err(source) => {
                            let launch_err = classify_session_error(source);
                            let text = launch_err.to_string();
                            emit_fatal(ui_tx, &fatal_emitted, text.clone());
                            return Err(anyhow::anyhow!(text));
                        }
                    }
                }
                None => {
                    let launch_err = classify_session_error(source);
                    let text = launch_err.to_string();
                    emit_fatal(ui_tx, &fatal_emitted, text.clone());
                    return Err(anyhow::anyhow!(text));
                }
            },
        },
    };
    let (session_config_options, session_config_targets) = initial_config.unwrap_or_default();
    let mut session_config = SessionConfigCache {
        options: session_config_options,
        targets: session_config_targets,
    };
    let _ = ui_tx.send(UiEvent::SessionStarted {
        session_id: session_id.to_string(),
        resumed,
    });
    if !session_config.options.is_empty() {
        let _ = ui_tx.send(UiEvent::SessionConfigOptions {
            options: session_config.options.clone(),
            targets: session_config.targets.clone(),
        });
    }

    while let Some(cmd) = ui_rx.recv().await {
        match cmd {
            UiCommand::SendPrompt { text, images } => {
                session_state.clear_permissions_cancelled(&session_id).await;
                if !drive_prompt_turn(
                    &conn,
                    &session_id,
                    text,
                    images,
                    ui_tx,
                    ui_rx,
                    &session_state,
                )
                .await?
                {
                    break;
                }
            }
            UiCommand::SetSessionConfigOption { target, value } => {
                if !drive_config_update(
                    &conn,
                    &session_id,
                    target,
                    value,
                    &mut session_config,
                    ui_tx,
                    ui_rx,
                )
                .await?
                {
                    break;
                }
            }
            UiCommand::ForkSession => {
                if !connected_fields.session_fork_supported {
                    let _ = ui_tx.send(UiEvent::Warning(
                        "session fork is not supported by this agent (unstable ACP extension not advertised)"
                            .to_string(),
                    ));
                    continue;
                }

                if !drive_fork_session(
                    &conn,
                    cwd.clone(),
                    &additional_directories,
                    &mut session_id,
                    &mut session_config,
                    &session_state,
                    ui_tx,
                    ui_rx,
                )
                .await?
                {
                    break;
                }
            }
            UiCommand::LoadSession {
                session_id: requested_session_id,
                cwd: requested_cwd,
                title,
                responder,
            } => {
                let target_session_id = SessionId::from(requested_session_id);
                if target_session_id == session_id {
                    if let Err(e) = session_state
                        .set_active_session_with_roots(
                            session_id.clone(),
                            &requested_cwd,
                            &additional_directories,
                        )
                        .await
                    {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: e.to_string(),
                        });
                        continue;
                    }
                    emit_connected(ui_tx, &connected_fields);
                    let _ = ui_tx.send(UiEvent::SessionStarted {
                        session_id: session_id.to_string(),
                        resumed: true,
                    });
                    let _ = ui_tx.send(UiEvent::SessionConfigOptions {
                        options: session_config.options.clone(),
                        targets: session_config.targets.clone(),
                    });
                    if let Some(title) = title {
                        let _ = ui_tx.send(UiEvent::SessionUpdate(
                            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
                        ));
                    }
                    let _ = responder.send(LoadSessionResult::Switched);
                    continue;
                }
                if init_resp
                    .agent_capabilities
                    .session_capabilities
                    .close
                    .is_none()
                {
                    let _ = responder.send(LoadSessionResult::Fallback {
                        message:
                            "agent does not advertise ACP capability sessionCapabilities.close"
                                .to_string(),
                    });
                    continue;
                }

                match switch_existing_session(
                    &conn,
                    &session_id,
                    target_session_id,
                    requested_cwd,
                    &additional_directories,
                    title,
                    &init_resp.agent_capabilities,
                    &init_resp.auth_methods,
                    &mut session_config,
                    &session_state,
                    &terminals,
                    &connected_fields,
                    ui_tx,
                )
                .await
                {
                    Ok(switched_session_id) => {
                        session_id = switched_session_id;
                        let _ = responder.send(LoadSessionResult::Switched);
                    }
                    Err(launch_err) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: launch_err.to_string(),
                        });
                    }
                }
            }
            UiCommand::CancelPrompt => {}
            UiCommand::Shutdown => break,
        }
    }
    Ok(())
}

fn emit_connected(ui_tx: &mpsc::UnboundedSender<UiEvent>, fields: &ConnectedEventFields) {
    let _ = ui_tx.send(UiEvent::Connected {
        agent_name: fields.agent_name.clone(),
        agent_version: fields.agent_version.clone(),
        prompt_images_supported: fields.prompt_images_supported,
        session_fork_supported: fields.session_fork_supported,
    });
}

#[allow(clippy::too_many_arguments)]
async fn switch_existing_session(
    conn: &ConnectionTo<Agent>,
    current_session_id: &SessionId,
    target_session_id: SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    title: Option<String>,
    capabilities: &AgentCapabilities,
    auth_methods: &[AuthMethod],
    session_config: &mut SessionConfigCache,
    session_state: &RuntimeSessionState,
    terminals: &ManagedTerminals,
    connected_fields: &ConnectedEventFields,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<SessionId, LaunchError> {
    require_resume_or_load_session(capabilities)?;
    close_session(conn, current_session_id.clone(), auth_methods).await?;
    session_state
        .mark_permissions_cancelled(current_session_id)
        .await;
    terminals.shutdown_session(current_session_id).await;
    session_state.clear_active_session().await;
    let loaded_config = resume_existing_session(
        conn,
        target_session_id.clone(),
        cwd.clone(),
        additional_directories,
        capabilities,
        auth_methods,
    )
    .await?;
    session_state
        .set_active_session_with_roots(target_session_id.clone(), &cwd, additional_directories)
        .await
        .map_err(|source| LaunchError::SessionCreateFailed { source })?;

    *session_config = loaded_config
        .map(|(options, targets)| SessionConfigCache { options, targets })
        .unwrap_or_else(|| SessionConfigCache {
            options: Vec::new(),
            targets: Vec::new(),
        });
    emit_connected(ui_tx, connected_fields);
    let _ = ui_tx.send(UiEvent::SessionStarted {
        session_id: target_session_id.to_string(),
        resumed: true,
    });
    let _ = ui_tx.send(UiEvent::SessionConfigOptions {
        options: session_config.options.clone(),
        targets: session_config.targets.clone(),
    });
    if let Some(title) = title {
        let _ = ui_tx.send(UiEvent::SessionUpdate(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title(title),
        )));
    }
    let _ = ui_tx.send(UiEvent::Info("session loaded".to_string()));
    Ok(target_session_id)
}

async fn close_session(
    conn: &ConnectionTo<Agent>,
    session_id: SessionId,
    auth_methods: &[AuthMethod],
) -> std::result::Result<(), LaunchError> {
    let close_req = CloseSessionRequest::new(session_id);
    match conn.send_request(close_req.clone()).block_task().await {
        Ok(_) => Ok(()),
        Err(source) => match auth_required_detail(&source) {
            Some(detail) => {
                authenticate_after_auth_required(conn, auth_methods, detail).await?;
                conn.send_request(close_req)
                    .block_task()
                    .await
                    .map(|_| ())
                    .map_err(classify_session_error)
            }
            None => Err(classify_session_error(source)),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_fork_session(
    conn: &ConnectionTo<Agent>,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
    session_id: &mut SessionId,
    session_config: &mut SessionConfigCache,
    session_state: &RuntimeSessionState,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
) -> Result<bool> {
    let source_session_id = session_id.clone();
    let fork = fork_session(
        conn,
        &source_session_id,
        cwd.clone(),
        additional_directories,
    );
    tokio::pin!(fork);

    loop {
        tokio::select! {
            result = &mut fork => {
                match result {
                    Ok((forked_session_id, forked_config)) => {
                        session_state
                            .set_active_session_with_roots(
                                forked_session_id.clone(),
                                &cwd,
                                additional_directories,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        *session_id = forked_session_id;
                        *session_config = forked_config.unwrap_or_else(|| SessionConfigCache {
                            options: Vec::new(),
                            targets: Vec::new(),
                        });
                        let _ = ui_tx.send(UiEvent::SessionStarted {
                            session_id: session_id.to_string(),
                            resumed: false,
                        });
                        let _ = ui_tx.send(UiEvent::SessionConfigOptions {
                            options: session_config.options.clone(),
                            targets: session_config.targets.clone(),
                        });
                        let _ = ui_tx.send(UiEvent::Info("session forked".to_string()));
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::SessionForkFailed {
                            message: format!("session fork failed: {e}"),
                        });
                    }
                }
                return Ok(true);
            }
            maybe_cmd = ui_rx.recv() => {
                match maybe_cmd {
                    Some(UiCommand::Shutdown) | None => {
                        return Ok(false);
                    }
                    Some(UiCommand::SendPrompt { .. }) => {
                        let _ = ui_tx.send(UiEvent::PromptFailed {
                            message: "prompt failed: session fork already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::SetSessionConfigOption { .. }) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSession) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::LoadSession { responder, .. }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "session fork already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::CancelPrompt) => {}
                }
            }
        }
    }
}

async fn fork_session(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    cwd: PathBuf,
    additional_directories: &[PathBuf],
) -> std::result::Result<(SessionId, Option<SessionConfigCache>), agent_client_protocol::Error> {
    let resp = conn
        .send_request(fork_session_request(
            session_id.clone(),
            cwd,
            additional_directories,
        ))
        .block_task()
        .await?;
    let config = session_config_from_parts(resp.config_options, resp.modes)
        .map(|(options, targets)| SessionConfigCache { options, targets });
    Ok((resp.session_id, config))
}

/// How a spawned agent relates to the controlling terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnIsolation {
    /// New process group, but keep the controlling terminal. The normal
    /// interactive/headless launch path.
    ProcessGroup,
    /// New session with **no controlling terminal** (`setsid` on Unix). Used
    /// by the startup probe: a backgrounded agent (and its `uvx`/`npx`
    /// grandchildren) must never read or write the user's TTY while the
    /// picker owns it.
    DetachedSession,
}

pub(crate) fn spawn_agent(
    command: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    stderr_path: Option<&std::path::Path>,
    isolation: SpawnIsolation,
) -> std::result::Result<
    (
        Child,
        tokio::process::ChildStdin,
        tokio::process::ChildStdout,
    ),
    LaunchError,
> {
    let command = normalize_spawn_program(command.to_path_buf());
    let mut cmd = Command::new(&command);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // If the runtime task is aborted, dropping the child should still terminate it.
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Place the agent into a new process group / Windows process group
    // so `kill_agent_tree` can reach every descendant on shutdown.
    // Without this, wrappers like `uvx brokk acp` (which fork a Python
    // interpreter to host the actual agent) leave that grandchild as
    // an orphan when mjolnir kills only the immediate child PID.
    #[cfg(unix)]
    {
        match isolation {
            SpawnIsolation::ProcessGroup => {
                cmd.process_group(0);
            }
            SpawnIsolation::DetachedSession => {
                // `setsid` (in the forked child, pre-exec) gives the agent a
                // brand-new session with no controlling terminal, so any
                // /dev/tty access — auth prompts, progress bars, job-control
                // tcsetpgrp — fails cleanly instead of stealing the user's
                // terminal input (SIGTTIN) or scribbling over the picker.
                // It also makes the child its own process-group leader
                // (pgid == pid), so `kill_agent_tree`'s killpg(pid) still
                // reaches the whole subtree.
                //
                // SAFETY: `setsid` is async-signal-safe and touches no Rust
                // state; the closure captures nothing.
                unsafe {
                    cmd.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
        }
    }
    #[cfg(windows)]
    {
        // Windows has no controlling-terminal / SIGTTIN semantics to detach
        // from, so both isolation modes use the same process group.
        let _ = isolation;
        // CREATE_NEW_PROCESS_GROUP from winbase.h. The child becomes
        // the root of a new group; `taskkill /T` walks the tree from
        // there.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    match stderr_path {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|source| LaunchError::StderrFileOpen {
                    path: path.to_path_buf(),
                    source,
                })?;
            cmd.stderr(std::process::Stdio::from(file));
        }
        None => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    let mut child = cmd.spawn().map_err(|e| classify_spawn_error(&command, e))?;
    // `stdin` / `stdout` are always Some here because we requested
    // `piped()` above; the `?` is just defensive.
    let stdin = child.stdin.take().ok_or_else(|| LaunchError::SpawnFailed {
        command: command.display().to_string(),
        source: std::io::Error::other("child stdin not piped"),
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LaunchError::SpawnFailed {
            command: command.display().to_string(),
            source: std::io::Error::other("child stdout not piped"),
        })?;
    Ok((child, stdin, stdout))
}

/// Kill the agent process and every descendant it spawned, then reap.
///
/// `spawn_agent` puts the child into a new process group (Unix) or new
/// Windows process group, so we can target the whole subtree here:
///
/// * **Unix** — `SIGTERM` the group for graceful exit, poll briefly for
///   the child to reap, then escalate to `SIGKILL` for any holdouts.
/// * **Windows** — `taskkill /T /F /PID <pid>` walks the parent/child
///   tree and force-terminates each process.
///
/// `agent_pid` is the value captured at spawn time. We can't rely on
/// `child.id()` here because the caller may have already reaped the
/// immediate child via `try_wait`/`wait` (in which case `id()` returns
/// `None`) — but the original PID is still a valid PGID handle for any
/// surviving grandchildren that inherited the group at fork time.
///
/// The trailing `child.kill().await` is a belt-and-braces step: it
/// reaps the immediate child if it survived the group/tree kill, and
/// is a no-op (ESRCH / "process not found") when it didn't. Failures
/// are logged but not propagated — by the time we reach shutdown the
/// caller has no meaningful recovery action.
pub(crate) async fn kill_agent_tree(child: &mut Child, agent_pid: Option<u32>) {
    if let Some(pid) = agent_pid {
        #[cfg(unix)]
        {
            // SAFETY: `killpg` is async-signal-safe and takes only a
            // pid_t plus an int; no Rust invariants involved. The PGID
            // equals the child's original PID because we spawned with
            // `process_group(0)`.
            unsafe {
                if libc::killpg(pid as libc::pid_t, libc::SIGTERM) != 0 {
                    let errno = std::io::Error::last_os_error();
                    // ESRCH just means the group is already gone.
                    if errno.raw_os_error() != Some(libc::ESRCH) {
                        tracing::warn!("killpg SIGTERM agent group {pid}: {errno}");
                    }
                }
            }
            // Up to ~250ms grace for the group to exit cleanly before
            // we SIGKILL. Keeps the exit fast while still giving
            // agents that flush state on SIGTERM a chance to do so.
            for _ in 0..5 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            unsafe {
                if libc::killpg(pid as libc::pid_t, libc::SIGKILL) != 0 {
                    let errno = std::io::Error::last_os_error();
                    if errno.raw_os_error() != Some(libc::ESRCH) {
                        tracing::warn!("killpg SIGKILL agent group {pid}: {errno}");
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            // /T = tree, /F = force. Targets the wrapper plus every
            // descendant it spawned (uvx -> python.exe, etc.).
            let status = tokio::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            if let Err(e) = status {
                tracing::warn!("taskkill agent pid {pid}: {e}");
            }
        }
    }

    if let Err(e) = child.kill().await {
        tracing::warn!("kill child: {e}");
    }
}

const DEFAULT_TERMINAL_OUTPUT_LIMIT: usize = 1024 * 1024;
pub(crate) const DEFAULT_FS_TEXT_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_CONFIGURABLE_FS_TEXT_BYTES: u64 = 64 * 1024 * 1024;
const FS_TEXT_SCAN_MULTIPLIER: u64 = 16;

#[derive(Clone, Copy)]
enum ReadSizePolicy {
    EnforceFileCap,
    AllowLargeFileForRange,
}

impl ReadSizePolicy {
    fn allows_large_file(self) -> bool {
        matches!(self, Self::AllowLargeFileForRange)
    }
}

struct LocalFileSystem {
    session_state: RuntimeSessionState,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    next_permission_id: AtomicU64,
    max_text_bytes: u64,
}

impl LocalFileSystem {
    fn new(
        session_state: RuntimeSessionState,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        max_text_bytes: u64,
    ) -> Self {
        Self {
            session_state,
            ui_tx,
            next_permission_id: AtomicU64::new(1),
            max_text_bytes,
        }
    }

    async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> std::result::Result<ReadTextFileResponse, agent_client_protocol::Error> {
        let roots = self
            .session_state
            .active_root_set(&request.session_id, "filesystem")
            .await?;
        let size_policy = if request.limit.is_some() {
            ReadSizePolicy::AllowLargeFileForRange
        } else {
            ReadSizePolicy::EnforceFileCap
        };
        let path = self
            .resolve_existing_file(&roots, &request.path, size_policy)
            .await?;
        let content =
            read_text_line_range_from_file(&path, request.line, request.limit, self.max_text_bytes)
                .await?;
        Ok(ReadTextFileResponse::new(content))
    }

    async fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> std::result::Result<WriteTextFileResponse, agent_client_protocol::Error> {
        let roots = self
            .session_state
            .active_root_set(&request.session_id, "filesystem")
            .await?;
        let content = request.content;
        if content.len() as u64 > self.max_text_bytes {
            return Err(fs_invalid_params(
                "filesystem write content exceeds client limit",
            ));
        }
        let path = self.resolve_write_path(&roots, &request.path).await?;
        self.confirm_write_permission(&request.session_id, &path, content.len())
            .await?;
        self.session_state
            .ensure_active_session(&request.session_id, "filesystem")
            .await?;
        let path = self.resolve_write_path(&roots, &path).await?;
        write_text_file_no_follow(&path, content)
            .await
            .map_err(|e| fs_io_error("write text file", &path, e, "file must be writable"))?;
        Ok(WriteTextFileResponse::new())
    }

    async fn resolve_existing_file(
        &self,
        roots: &[PathBuf],
        path: &Path,
        size_policy: ReadSizePolicy,
    ) -> std::result::Result<PathBuf, agent_client_protocol::Error> {
        self.validate_absolute(path)?;
        let path = tokio::fs::canonicalize(path)
            .await
            .map_err(|e| fs_io_error("resolve text file", path, e, "file must exist"))?;
        self.validate_under_any_root(roots, &path)?;
        let metadata = tokio::fs::metadata(&path).await.map_err(|e| {
            fs_io_error(
                "inspect text file",
                &path,
                e,
                "file metadata must be readable",
            )
        })?;
        if !metadata.is_file() {
            return Err(fs_invalid_params("filesystem path is not a regular file"));
        }
        if !size_policy.allows_large_file() && metadata.len() > self.max_text_bytes {
            return Err(fs_invalid_params(
                "filesystem read file exceeds client limit",
            ));
        }
        Ok(path)
    }

    async fn resolve_write_path(
        &self,
        roots: &[PathBuf],
        path: &Path,
    ) -> std::result::Result<PathBuf, agent_client_protocol::Error> {
        self.validate_absolute(path)?;
        if path.file_name().is_none() {
            return Err(fs_invalid_params("filesystem write path must name a file"));
        }

        match tokio::fs::canonicalize(path).await {
            Ok(existing) => {
                self.validate_under_any_root(roots, &existing)?;
                let metadata = tokio::fs::metadata(&existing).await.map_err(|e| {
                    fs_io_error(
                        "inspect text file",
                        &existing,
                        e,
                        "file metadata must be readable",
                    )
                })?;
                if !metadata.is_file() {
                    return Err(fs_invalid_params("filesystem path is not a regular file"));
                }
                Ok(existing)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or_else(|| {
                    fs_invalid_params("filesystem write path must have a parent directory")
                })?;
                let parent = tokio::fs::canonicalize(parent).await.map_err(|e| {
                    fs_io_error("resolve parent directory", parent, e, "parent must exist")
                })?;
                self.validate_under_any_root(roots, &parent)?;
                Ok(parent.join(path.file_name().expect("checked above")))
            }
            Err(e) => Err(fs_io_error(
                "resolve text file",
                path,
                e,
                "file path must be resolvable",
            )),
        }
    }

    fn validate_absolute(
        &self,
        path: &Path,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if path.is_absolute() {
            Ok(())
        } else {
            Err(fs_invalid_params(format!(
                "filesystem path must be absolute: {}",
                path.display()
            )))
        }
    }

    fn validate_under_any_root(
        &self,
        roots: &[PathBuf],
        path: &Path,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if path_is_under_any_root(roots, path) {
            Ok(())
        } else {
            Err(fs_invalid_params(
                "filesystem path is outside active workspace roots",
            ))
        }
    }

    async fn confirm_write_permission(
        &self,
        session_id: &SessionId,
        path: &Path,
        bytes: usize,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        let request_id = format!(
            "mj-fs-write-{}",
            self.next_permission_id.fetch_add(1, Ordering::Relaxed)
        );
        let mut fields = ToolCallUpdateFields::new();
        fields.kind = Some(ToolKind::Edit);
        fields.status = Some(ToolCallStatus::Pending);
        fields.title = Some(format!("write {}", path.display()));
        fields.raw_input = Some(serde_json::json!({
            "path": path.display().to_string(),
            "bytes": bytes,
        }));
        let (tx, rx) = oneshot::channel::<PermissionDecision>();
        let prompt = PermissionPrompt {
            tool_call: ToolCallUpdate::new(request_id, fields),
            options: vec![
                PermissionOption::new("allow", "Allow write", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
            responder: tx,
        };
        if self.ui_tx.send(UiEvent::PermissionRequest(prompt)).is_err() {
            return Err(agent_client_protocol::Error::internal_error().data(
                serde_json::Value::String("permission UI unavailable".to_string()),
            ));
        }
        match rx.await {
            Ok(PermissionDecision::Selected(option)) if option == "allow" => Ok(()),
            _ => Err(agent_client_protocol::Error::invalid_request().data(
                serde_json::Value::String("filesystem write denied".to_string()),
            )),
        }?;
        self.session_state
            .ensure_active_session(session_id, "filesystem")
            .await
    }
}

async fn write_text_file_no_follow(path: &Path, content: String) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .await?;
        file.write_all(content.as_bytes()).await?;
        file.flush().await
    }

    #[cfg(not(unix))]
    {
        tokio::fs::write(path, content).await
    }
}

async fn read_text_line_range_from_file(
    path: &Path,
    line: Option<u32>,
    limit: Option<u32>,
    max_text_bytes: u64,
) -> std::result::Result<String, agent_client_protocol::Error> {
    let (start, limit) = line_range_window(line, limit)?;
    if limit == Some(0) {
        return Ok(String::new());
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| fs_io_error("read text file", path, e, "file must exist"))?;
    let mut reader = BufReader::new(file);
    let mut content = Vec::new();
    let mut index = 0_usize;
    let mut scanned_bytes = 0_u64;
    let max_scan_bytes = fs_text_scan_byte_limit(max_text_bytes);

    loop {
        let mut done = false;
        let consumed = {
            let buffer = reader
                .fill_buf()
                .await
                .map_err(|e| fs_io_error("read text file", path, e, "file must be readable"))?;
            if buffer.is_empty() {
                break;
            }

            let mut consumed = 0_usize;
            while consumed < buffer.len() {
                let remaining = &buffer[consumed..];
                let segment_len = remaining
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(remaining.len(), |newline| newline + 1);
                let segment = &remaining[..segment_len];

                scanned_bytes = scanned_bytes.saturating_add(segment.len() as u64);
                if scanned_bytes > max_scan_bytes {
                    return Err(fs_invalid_params(
                        "filesystem read scan exceeds client limit",
                    ));
                }

                let in_range = index >= start && limit.is_none_or(|limit| index - start < limit);
                if in_range {
                    if (content.len() + segment.len()) as u64 > max_text_bytes {
                        return Err(fs_invalid_params(
                            "filesystem read response exceeds client limit",
                        ));
                    }
                    content.extend_from_slice(segment);
                }

                consumed += segment_len;
                if segment.ends_with(b"\n") {
                    index += 1;
                    if limit.is_some_and(|limit| index >= start.saturating_add(limit)) {
                        done = true;
                        break;
                    }
                }
            }
            consumed
        };
        reader.consume(consumed);
        if done {
            break;
        }
    }

    String::from_utf8(content).map_err(|e| {
        fs_io_error(
            "read text file",
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            "file must contain valid UTF-8",
        )
    })
}

fn fs_text_scan_byte_limit(max_text_bytes: u64) -> u64 {
    max_text_bytes
        .saturating_mul(FS_TEXT_SCAN_MULTIPLIER)
        .clamp(DEFAULT_FS_TEXT_BYTES, MAX_CONFIGURABLE_FS_TEXT_BYTES)
}

fn line_range_window(
    line: Option<u32>,
    limit: Option<u32>,
) -> std::result::Result<(usize, Option<usize>), agent_client_protocol::Error> {
    let start = match line {
        Some(0) => return Err(fs_invalid_params("filesystem read line must be 1-based")),
        Some(line) => line.saturating_sub(1) as usize,
        None => 0,
    };
    Ok((start, limit.map(|limit| limit as usize)))
}

#[cfg(test)]
fn read_text_line_range(
    content: &str,
    line: Option<u32>,
    limit: Option<u32>,
) -> std::result::Result<String, agent_client_protocol::Error> {
    let (start, limit) = line_range_window(line, limit)?;
    let lines = content.split_inclusive('\n').skip(start);
    let selected = match limit {
        Some(limit) => lines.take(limit).collect(),
        None => lines.collect(),
    };
    Ok(selected)
}

fn fs_invalid_params(message: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params()
        .data(serde_json::Value::String(message.to_string()))
}

fn fs_io_error(
    action: &str,
    path: &Path,
    error: std::io::Error,
    hint: &str,
) -> agent_client_protocol::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        return agent_client_protocol::Error::resource_not_found(Some(path.display().to_string()));
    }
    fs_invalid_params(format!(
        "{action} failed for {}: {error}; {hint}",
        path.display()
    ))
}

struct ManagedTerminals {
    terminals: Mutex<HashMap<String, Arc<ManagedTerminal>>>,
    next_id: AtomicU64,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    session_state: Option<RuntimeSessionState>,
}

#[derive(Debug)]
struct ManagedTerminal {
    session_id: SessionId,
    terminal_id: String,
    pid: Option<u32>,
    output: Arc<Mutex<TerminalOutputBuffer>>,
    exit_rx: watch::Receiver<Option<TerminalExitStatus>>,
}

#[derive(Debug)]
struct TerminalOutputBuffer {
    output: String,
    truncated: bool,
    limit: usize,
}

impl TerminalOutputBuffer {
    fn new(limit: usize) -> Self {
        Self {
            output: String::new(),
            truncated: false,
            limit,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.output.push_str(&String::from_utf8_lossy(bytes));
        self.truncate_to_limit();
    }

    fn truncate_to_limit(&mut self) {
        if self.output.len() <= self.limit {
            return;
        }
        self.truncated = true;
        if self.limit == 0 {
            self.output.clear();
            return;
        }

        let mut start = self.output.len().saturating_sub(self.limit);
        while start < self.output.len() && !self.output.is_char_boundary(start) {
            start += 1;
        }
        self.output.drain(..start);
    }
}

impl ManagedTerminals {
    #[cfg(test)]
    fn new(ui_tx: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ui_tx,
            session_state: None,
        }
    }

    fn with_session_state(
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        session_state: RuntimeSessionState,
    ) -> Self {
        Self {
            terminals: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            ui_tx,
            session_state: Some(session_state),
        }
    }

    async fn create(
        &self,
        request: CreateTerminalRequest,
    ) -> std::result::Result<CreateTerminalResponse, agent_client_protocol::Error> {
        self.validate_active_session(&request.session_id).await?;
        if request.command.trim().is_empty() {
            return Err(terminal_invalid_params("terminal command cannot be empty"));
        }

        let terminal_id = format!("mj-term-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let output_limit = request
            .output_byte_limit
            .and_then(|limit| usize::try_from(limit).ok())
            .unwrap_or(DEFAULT_TERMINAL_OUTPUT_LIMIT);
        let output = Arc::new(Mutex::new(TerminalOutputBuffer::new(output_limit)));
        let (exit_tx, exit_rx) = watch::channel(None);

        let mut cmd = Command::new(&request.command);
        cmd.args(&request.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = self.resolve_terminal_cwd(&request).await? {
            cmd.current_dir(cwd);
        }
        for env in &request.env {
            cmd.env(&env.name, &env.value);
        }
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let mut child = cmd.spawn().map_err(|e| {
            terminal_invalid_params(format!("failed to spawn terminal command: {e}"))
        })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let terminal = Arc::new(ManagedTerminal {
            session_id: request.session_id,
            terminal_id: terminal_id.clone(),
            pid,
            output: output.clone(),
            exit_rx,
        });
        self.terminals
            .lock()
            .await
            .insert(terminal_id.clone(), terminal);

        let mut reader_tasks = Vec::new();
        if let Some(stdout) = stdout {
            reader_tasks.push(tokio::spawn(read_terminal_stream(
                stdout,
                terminal_id.clone(),
                output.clone(),
                self.ui_tx.clone(),
                None,
            )));
        }
        if let Some(stderr) = stderr {
            reader_tasks.push(tokio::spawn(read_terminal_stream(
                stderr,
                terminal_id.clone(),
                output.clone(),
                self.ui_tx.clone(),
                None,
            )));
        }

        tokio::spawn(wait_terminal_child(
            child,
            terminal_id.clone(),
            output,
            self.ui_tx.clone(),
            exit_tx,
            reader_tasks,
        ));

        Ok(CreateTerminalResponse::new(TerminalId::new(terminal_id)))
    }

    async fn resolve_terminal_cwd(
        &self,
        request: &CreateTerminalRequest,
    ) -> std::result::Result<Option<PathBuf>, agent_client_protocol::Error> {
        let Some(session_state) = &self.session_state else {
            if let Some(cwd) = &request.cwd
                && !cwd.is_absolute()
            {
                return Err(terminal_invalid_params(
                    "terminal cwd must be an absolute path",
                ));
            }
            return Ok(request.cwd.clone());
        };
        let roots = session_state
            .active_root_set(&request.session_id, "terminal")
            .await?;
        let cwd = match &request.cwd {
            Some(cwd) => {
                if !cwd.is_absolute() {
                    return Err(terminal_invalid_params(
                        "terminal cwd must be an absolute path",
                    ));
                }
                tokio::fs::canonicalize(cwd).await.map_err(|e| {
                    terminal_invalid_params(format!(
                        "terminal cwd must exist and be accessible: {e}"
                    ))
                })?
            }
            None => roots[0].clone(),
        };
        if path_is_under_any_root(&roots, &cwd) {
            Ok(Some(cwd))
        } else {
            Err(terminal_invalid_params(
                "terminal cwd is outside active workspace roots",
            ))
        }
    }

    async fn output(
        &self,
        request: TerminalOutputRequest,
    ) -> std::result::Result<TerminalOutputResponse, agent_client_protocol::Error> {
        let terminal = self
            .get_terminal(&request.session_id, &request.terminal_id)
            .await?;
        let snapshot = terminal.snapshot().await;
        Ok(
            TerminalOutputResponse::new(snapshot.output, snapshot.truncated)
                .exit_status(snapshot.exit_status),
        )
    }

    async fn release(
        &self,
        request: ReleaseTerminalRequest,
    ) -> std::result::Result<ReleaseTerminalResponse, agent_client_protocol::Error> {
        let terminal = self
            .remove_terminal(&request.session_id, &request.terminal_id)
            .await?;
        if terminal.exit_rx.borrow().is_none() {
            kill_terminal_process(terminal.pid).await.map_err(|e| {
                agent_client_protocol::Error::internal_error().data(serde_json::Value::String(e))
            })?;
        }
        Ok(ReleaseTerminalResponse::new())
    }

    async fn wait_for_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> std::result::Result<WaitForTerminalExitResponse, agent_client_protocol::Error> {
        let terminal = self
            .get_terminal(&request.session_id, &request.terminal_id)
            .await?;
        let exit_status = terminal.wait_for_exit().await?;
        Ok(WaitForTerminalExitResponse::new(exit_status))
    }

    async fn kill(
        &self,
        request: KillTerminalRequest,
    ) -> std::result::Result<KillTerminalResponse, agent_client_protocol::Error> {
        let terminal = self
            .get_terminal(&request.session_id, &request.terminal_id)
            .await?;
        if terminal.exit_rx.borrow().is_none() {
            kill_terminal_process(terminal.pid).await.map_err(|e| {
                agent_client_protocol::Error::internal_error().data(serde_json::Value::String(e))
            })?;
        }
        Ok(KillTerminalResponse::new())
    }

    async fn get_terminal(
        &self,
        session_id: &SessionId,
        terminal_id: &TerminalId,
    ) -> std::result::Result<Arc<ManagedTerminal>, agent_client_protocol::Error> {
        self.validate_active_session(session_id).await?;
        let key = terminal_id.to_string();
        let Some(terminal) = self.terminals.lock().await.get(&key).cloned() else {
            return Err(terminal_invalid_params(format!(
                "unknown terminal id: {key}"
            )));
        };
        terminal.validate_session(session_id)?;
        Ok(terminal)
    }

    async fn remove_terminal(
        &self,
        session_id: &SessionId,
        terminal_id: &TerminalId,
    ) -> std::result::Result<Arc<ManagedTerminal>, agent_client_protocol::Error> {
        self.validate_active_session(session_id).await?;
        let key = terminal_id.to_string();
        let mut terminals = self.terminals.lock().await;
        let Some(terminal) = terminals.get(&key).cloned() else {
            return Err(terminal_invalid_params(format!(
                "unknown terminal id: {key}"
            )));
        };
        terminal.validate_session(session_id)?;
        terminals.remove(&key);
        Ok(terminal)
    }

    async fn validate_active_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        let Some(session_state) = &self.session_state else {
            return Ok(());
        };
        session_state
            .ensure_active_session(session_id, "terminal")
            .await
    }

    async fn shutdown_session(&self, session_id: &SessionId) {
        let terminals: Vec<Arc<ManagedTerminal>> = {
            let mut terminals = self.terminals.lock().await;
            let keys = terminals
                .iter()
                .filter(|(_, terminal)| terminal.session_id == *session_id)
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| terminals.remove(&key))
                .collect()
        };
        for terminal in terminals {
            if terminal.exit_rx.borrow().is_none()
                && let Err(e) = kill_terminal_process(terminal.pid).await
            {
                tracing::warn!("shutdown terminal {}: {e}", terminal.terminal_id);
            }
        }
    }

    async fn shutdown_all(&self) {
        let terminals: Vec<Arc<ManagedTerminal>> = self
            .terminals
            .lock()
            .await
            .drain()
            .map(|(_, t)| t)
            .collect();
        for terminal in terminals {
            if terminal.exit_rx.borrow().is_none()
                && let Err(e) = kill_terminal_process(terminal.pid).await
            {
                tracing::warn!("shutdown terminal {}: {e}", terminal.terminal_id);
            }
        }
    }
}

impl ManagedTerminal {
    fn validate_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<(), agent_client_protocol::Error> {
        if &self.session_id != session_id {
            return Err(terminal_invalid_params(format!(
                "terminal {} does not belong to session {}",
                self.terminal_id, session_id
            )));
        }
        Ok(())
    }

    async fn snapshot(&self) -> TerminalOutputSnapshot {
        let output = self.output.lock().await;
        TerminalOutputSnapshot {
            terminal_id: self.terminal_id.clone(),
            output: output.output.clone(),
            truncated: output.truncated,
            exit_status: self.exit_rx.borrow().clone(),
        }
    }

    async fn wait_for_exit(
        &self,
    ) -> std::result::Result<TerminalExitStatus, agent_client_protocol::Error> {
        let mut rx = self.exit_rx.clone();
        loop {
            if let Some(status) = rx.borrow().clone() {
                return Ok(status);
            }
            rx.changed().await.map_err(|_| {
                agent_client_protocol::Error::internal_error().data(serde_json::Value::String(
                    "terminal wait task ended".to_string(),
                ))
            })?;
        }
    }
}

async fn read_terminal_stream<R>(
    mut stream: R,
    terminal_id: String,
    output: Arc<Mutex<TerminalOutputBuffer>>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    exit_status: Option<TerminalExitStatus>,
) where
    R: AsyncRead + Unpin,
{
    let mut buf = [0_u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let snapshot = {
                    let mut output = output.lock().await;
                    output.append(&buf[..n]);
                    TerminalOutputSnapshot {
                        terminal_id: terminal_id.clone(),
                        output: output.output.clone(),
                        truncated: output.truncated,
                        exit_status: exit_status.clone(),
                    }
                };
                let _ = ui_tx.send(UiEvent::TerminalOutput(snapshot));
            }
            Err(e) => {
                tracing::warn!("read terminal {terminal_id} output: {e}");
                break;
            }
        }
    }
}

async fn wait_terminal_child(
    mut child: Child,
    terminal_id: String,
    output: Arc<Mutex<TerminalOutputBuffer>>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    exit_tx: watch::Sender<Option<TerminalExitStatus>>,
    reader_tasks: Vec<tokio::task::JoinHandle<()>>,
) {
    let status = match child.wait().await {
        Ok(status) => terminal_exit_status(status),
        Err(e) => {
            tracing::warn!("wait terminal {terminal_id}: {e}");
            TerminalExitStatus::new().signal("wait_error")
        }
    };
    for task in reader_tasks {
        if let Err(e) = task.await {
            tracing::warn!("join terminal {terminal_id} reader: {e}");
        }
    }
    let _ = exit_tx.send(Some(status.clone()));
    let snapshot = {
        let output = output.lock().await;
        TerminalOutputSnapshot {
            terminal_id,
            output: output.output.clone(),
            truncated: output.truncated,
            exit_status: Some(status),
        }
    };
    let _ = ui_tx.send(UiEvent::TerminalOutput(snapshot));
}

fn terminal_exit_status(status: std::process::ExitStatus) -> TerminalExitStatus {
    let mut exit = TerminalExitStatus::new();
    if let Some(code) = status.code().and_then(|code| u32::try_from(code).ok()) {
        exit = exit.exit_code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            exit = exit.signal(signal_name(signal));
        }
    }
    exit
}

#[cfg(unix)]
fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGTERM => "SIGTERM".to_string(),
        libc::SIGKILL => "SIGKILL".to_string(),
        libc::SIGINT => "SIGINT".to_string(),
        libc::SIGHUP => "SIGHUP".to_string(),
        _ => format!("SIG{signal}"),
    }
}

async fn kill_terminal_process(pid: Option<u32>) -> std::result::Result<(), String> {
    let Some(pid) = pid else {
        return Ok(());
    };

    #[cfg(unix)]
    {
        unsafe {
            if libc::killpg(pid as libc::pid_t, libc::SIGTERM) != 0 {
                let errno = std::io::Error::last_os_error();
                if errno.raw_os_error() != Some(libc::ESRCH) {
                    return Err(format!("kill terminal group {pid} with SIGTERM: {errno}"));
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        unsafe {
            if libc::killpg(pid as libc::pid_t, libc::SIGKILL) != 0 {
                let errno = std::io::Error::last_os_error();
                if errno.raw_os_error() != Some(libc::ESRCH) {
                    return Err(format!("kill terminal group {pid} with SIGKILL: {errno}"));
                }
            }
        }
        Ok(())
    }

    #[cfg(windows)]
    {
        let status = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| format!("taskkill terminal pid {pid}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("taskkill terminal pid {pid} exited with {status}"))
        }
    }
}

fn terminal_invalid_params(message: impl ToString) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params()
        .data(serde_json::Value::String(message.to_string()))
}

#[cfg(test)]
fn terminal_test_command(script: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), script.to_string()],
        )
    }
    #[cfg(not(windows))]
    {
        ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
    }
}

fn session_config_from_parts(
    config_options: Option<Vec<SessionConfigOption>>,
    modes: Option<SessionModeState>,
) -> Option<(Vec<SessionConfigOption>, Vec<SessionConfigTarget>)> {
    if let Some(options) = config_options
        && !options.is_empty()
    {
        let targets = config_option_targets(&options);
        return Some((options, targets));
    }

    let mut options = Vec::new();
    let mut targets = Vec::new();

    if let Some(modes) = modes
        && let Some(option) = legacy_mode_config_option(modes)
    {
        options.push(option);
        targets.push(SessionConfigTarget::LegacyMode);
    }

    (!options.is_empty()).then_some((options, targets))
}

fn config_option_targets(options: &[SessionConfigOption]) -> Vec<SessionConfigTarget> {
    options
        .iter()
        .map(|option| SessionConfigTarget::ConfigOption {
            config_id: option.id.clone(),
        })
        .collect()
}

fn legacy_mode_config_option(modes: SessionModeState) -> Option<SessionConfigOption> {
    if modes.available_modes.is_empty() {
        return None;
    }

    let is_thinking = modes
        .available_modes
        .iter()
        .all(|mode| mode.name.starts_with("Thinking:"));
    let name = if is_thinking { "Thinking" } else { "Mode" };
    let category = if is_thinking {
        SessionConfigOptionCategory::ThoughtLevel
    } else {
        SessionConfigOptionCategory::Mode
    };
    let options = modes
        .available_modes
        .into_iter()
        .map(|mode| {
            SessionConfigSelectOption::new(mode.id.to_string(), mode.name)
                .description(mode.description)
        })
        .collect::<Vec<_>>();

    Some(
        SessionConfigOption::select(
            name.to_ascii_lowercase(),
            name,
            modes.current_mode_id.to_string(),
            options,
        )
        .category(category),
    )
}

fn set_current_config_value(
    options: &mut [SessionConfigOption],
    targets: &[SessionConfigTarget],
    target: &SessionConfigTarget,
    value: &SessionConfigValueId,
) {
    let Some(option) = targets
        .iter()
        .position(|candidate| candidate == target)
        .and_then(|index| options.get_mut(index))
    else {
        return;
    };

    if let SessionConfigKind::Select(select) = &mut option.kind {
        select.current_value = value.clone();
    }
}

struct SessionConfigCache {
    options: Vec<SessionConfigOption>,
    targets: Vec<SessionConfigTarget>,
}

async fn drive_config_update(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    target: SessionConfigTarget,
    value: agent_client_protocol::schema::v1::SessionConfigValueId,
    session_config: &mut SessionConfigCache,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
) -> Result<bool> {
    let update = send_config_update(conn, session_id, target.clone(), value.clone());
    tokio::pin!(update);

    loop {
        tokio::select! {
            result = &mut update => {
                match result {
                    Ok(Some(options)) => {
                        session_config.targets = config_option_targets(&options);
                        session_config.options = options;
                        let _ = ui_tx.send(UiEvent::SessionConfigOptions {
                            options: session_config.options.clone(),
                            targets: session_config.targets.clone(),
                        });
                    }
                    Ok(None) => {
                        set_current_config_value(
                            &mut session_config.options,
                            &session_config.targets,
                            &target,
                            &value,
                        );
                        let _ = ui_tx.send(UiEvent::SessionConfigOptions {
                            options: session_config.options.clone(),
                            targets: session_config.targets.clone(),
                        });
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Warning(format!(
                            "session config update failed: {e}"
                        )));
                    }
                }
                return Ok(true);
            }
            maybe_cmd = ui_rx.recv() => {
                match maybe_cmd {
                    Some(UiCommand::Shutdown) | None => {
                        return Ok(false);
                    }
                    Some(UiCommand::SendPrompt { .. }) => {
                        let _ = ui_tx.send(UiEvent::PromptFailed {
                            message: "prompt failed: config update already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::SetSessionConfigOption { .. }) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "config update already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSession) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork is only supported while idle".to_string(),
                        ));
                    }
                    Some(UiCommand::LoadSession { responder, .. }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "config update already in flight".to_string(),
                        });
                    }
                    Some(UiCommand::CancelPrompt) => {}
                }
            }
        }
    }
}

async fn send_config_update(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    target: SessionConfigTarget,
    value: SessionConfigValueId,
) -> std::result::Result<Option<Vec<SessionConfigOption>>, agent_client_protocol::Error> {
    match target {
        SessionConfigTarget::ConfigOption { config_id } => {
            let req = SetSessionConfigOptionRequest::new(session_id.clone(), config_id, value);
            conn.send_request(req)
                .block_task()
                .await
                .map(|resp| Some(resp.config_options))
        }
        SessionConfigTarget::LegacyModel => Err(legacy_model_config_update_error()),
        SessionConfigTarget::LegacyMode => {
            let req = SetSessionModeRequest::new(session_id.clone(), value.to_string());
            conn.send_request(req).block_task().await.map(|_| None)
        }
    }
}

fn legacy_model_config_update_error() -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(serde_json::json!({
        "target": "legacy_model",
        "reason": "legacy session model updates are not supported by agent-client-protocol 0.14",
    }))
}

async fn drive_prompt_turn(
    conn: &ConnectionTo<Agent>,
    session_id: &SessionId,
    text: String,
    images: Vec<PromptImage>,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
    ui_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    session_state: &RuntimeSessionState,
) -> Result<bool> {
    let req = PromptRequest::new(session_id.clone(), prompt_content_blocks(text, images));
    let prompt = conn.send_request(req).block_task();
    tokio::pin!(prompt);

    let mut cancel_sent = false;
    loop {
        tokio::select! {
            prompt_result = &mut prompt => {
                match prompt_result {
                    Ok(resp) => {
                        let _ = ui_tx.send(UiEvent::PromptDone {
                            stop_reason: resp.stop_reason,
                            usage: resp.usage,
                        });
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::PromptFailed {
                            message: format!("prompt failed: {e}"),
                        });
                    }
                }
                return Ok(true);
            }
            maybe_cmd = ui_rx.recv() => {
                match maybe_cmd {
                    Some(UiCommand::CancelPrompt) => {
                        if !cancel_sent {
                            session_state.mark_permissions_cancelled(session_id).await;
                            let _ = ui_tx.send(UiEvent::CancelPendingPermissions);
                            if let Err(e) = conn.send_notification(CancelNotification::new(session_id.clone())) {
                                let _ = ui_tx.send(UiEvent::Warning(format!("cancel failed: {e}")));
                            }
                            cancel_sent = true;
                        }
                    }
                    Some(UiCommand::Shutdown) | None => {
                        return Ok(false);
                    }
                    Some(UiCommand::SendPrompt { .. }) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "prompt already in flight".to_string(),
                        ));
                    }
                    Some(UiCommand::SetSessionConfigOption { .. }) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "config updates are only supported while idle".to_string(),
                        ));
                    }
                    Some(UiCommand::ForkSession) => {
                        let _ = ui_tx.send(UiEvent::Warning(
                            "session fork is only supported while idle".to_string(),
                        ));
                    }
                    Some(UiCommand::LoadSession { responder, .. }) => {
                        let _ = responder.send(LoadSessionResult::Fallback {
                            message: "prompt already in flight".to_string(),
                        });
                    }
                }
            }
        }
    }
}

fn prompt_content_blocks(text: String, images: Vec<PromptImage>) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text(TextContent::new(text)));
    }
    content.extend(
        images.into_iter().map(|image| {
            ContentBlock::Image(ImageContent::new(image.data_base64, image.mime_type))
        }),
    );
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use agent_client_protocol::Agent as AgentRole;
    use agent_client_protocol::schema::v1::{
        AuthMethodAgent, AuthenticateResponse, CloseSessionResponse, ContentBlock, ContentChunk,
        ForkSessionResponse, InitializeResponse, LoadSessionResponse, NewSessionResponse,
        PermissionOption, PermissionOptionKind, PromptResponse, ResumeSessionResponse,
        SessionAdditionalDirectoriesCapabilities, SessionCapabilities, SessionCloseCapabilities,
        SessionConfigId, SessionConfigValueId, SessionForkCapabilities, SessionId,
        SessionNotification, SessionResumeCapabilities, SessionUpdate,
        SetSessionConfigOptionRequest, StopReason, TextContent, ToolCallUpdate,
        ToolCallUpdateFields,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool as StdAtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::io::split;

    #[test]
    fn resolve_no_install_returns_none_for_missing_program() {
        let resolved = resolve_agent_command_no_install(
            &PathBuf::from("definitely-not-a-real-program-xyzzy"),
            &HashMap::new(),
        );
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_no_install_resolves_existing_path_and_keeps_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("agent-bin");
        std::fs::write(&bin, b"#!/bin/sh\n").expect("write bin");

        let env = HashMap::from([("FOO".to_string(), "bar".to_string())]);
        let resolved = resolve_agent_command_no_install(&bin, &env).expect("resolve");
        assert_eq!(resolved.command, bin);
        assert_eq!(resolved.env.get("FOO"), Some(&"bar".to_string()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_session_child_becomes_session_leader() {
        // The startup probe spawns agents with `DetachedSession` so they get
        // their own session with no controlling terminal — the guard against
        // a backgrounded agent stealing the picker's TTY (SIGTTIN / tty
        // corruption). A session leader has sid == pid.
        let (mut child, _stdin, _stdout) = spawn_agent(
            &PathBuf::from("sleep"),
            &["5".to_string()],
            &HashMap::new(),
            None,
            SpawnIsolation::DetachedSession,
        )
        .expect("spawn sleep");
        let pid = child.id().expect("pid") as libc::pid_t;

        // setsid runs in the child between fork and exec, so poll briefly to
        // avoid racing the exec.
        let mut sid = -1;
        for _ in 0..100 {
            sid = unsafe { libc::getsid(pid) };
            if sid == pid {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(sid, pid, "detached child should be its own session leader");

        kill_agent_tree(&mut child, Some(pid as u32)).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_child_stays_in_our_session() {
        // The normal launch path keeps the controlling terminal: the child
        // shares our session and is not itself a session leader.
        let (mut child, _stdin, _stdout) = spawn_agent(
            &PathBuf::from("sleep"),
            &["5".to_string()],
            &HashMap::new(),
            None,
            SpawnIsolation::ProcessGroup,
        )
        .expect("spawn sleep");
        let pid = child.id().expect("pid") as libc::pid_t;

        let our_sid = unsafe { libc::getsid(0) };
        let child_sid = unsafe { libc::getsid(pid) };
        assert_eq!(child_sid, our_sid, "process-group child shares our session");
        assert_ne!(pid, child_sid, "and is not a session leader");

        kill_agent_tree(&mut child, Some(pid as u32)).await;
    }

    #[test]
    fn prompt_content_blocks_include_text_and_images() {
        let blocks = prompt_content_blocks(
            "look".to_string(),
            vec![PromptImage {
                data_base64: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
                width: 640,
                height: 480,
            }],
        );

        assert_eq!(blocks.len(), 2);
        match &blocks[0] {
            ContentBlock::Text(text) => assert_eq!(text.text, "look"),
            other => panic!("unexpected text block: {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::Image(image) => {
                assert_eq!(image.data, "aW1hZ2U=");
                assert_eq!(image.mime_type, "image/png");
            }
            other => panic!("unexpected image block: {other:?}"),
        }
    }

    #[test]
    fn read_text_line_range_uses_one_based_lines_and_preserves_newlines() {
        let content = "alpha\nbeta\ngamma\n";

        assert_eq!(
            read_text_line_range(content, Some(2), Some(2)).expect("slice"),
            "beta\ngamma\n"
        );
        assert_eq!(
            read_text_line_range(content, Some(4), None).expect("past end"),
            ""
        );
        assert!(read_text_line_range(content, Some(0), Some(1)).is_err());
    }

    async fn test_filesystem(
        root: &Path,
        session_id: &SessionId,
    ) -> (
        LocalFileSystem,
        mpsc::UnboundedReceiver<UiEvent>,
        RuntimeSessionState,
    ) {
        test_filesystem_with_limit(root, session_id, DEFAULT_FS_TEXT_BYTES).await
    }

    async fn test_filesystem_with_limit(
        root: &Path,
        session_id: &SessionId,
        max_text_bytes: u64,
    ) -> (
        LocalFileSystem,
        mpsc::UnboundedReceiver<UiEvent>,
        RuntimeSessionState,
    ) {
        let state = RuntimeSessionState::new();
        state
            .set_active_session(session_id.clone(), root)
            .await
            .expect("active session");
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        (
            LocalFileSystem::new(state.clone(), ui_tx, max_text_bytes),
            ui_rx,
            state,
        )
    }

    async fn allow_next_permission(ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>) {
        let ev = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
            .await
            .expect("permission event")
            .expect("permission event");
        match ev {
            UiEvent::PermissionRequest(prompt) => prompt
                .responder
                .send(PermissionDecision::Selected("allow".to_string()))
                .expect("send permission decision"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn local_filesystem_reads_and_writes_inside_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, "one\ntwo\nthree\n")
            .await
            .expect("seed file");
        let (filesystem, mut ui_rx, _state) = test_filesystem(temp.path(), &session_id).await;

        let read = filesystem
            .read_text_file(
                ReadTextFileRequest::new(session_id.clone(), path.clone())
                    .line(2)
                    .limit(1),
            )
            .await
            .expect("read");
        assert_eq!(read.content, "two\n");

        let write_path = temp.path().join("created.txt");
        let write = filesystem.write_text_file(WriteTextFileRequest::new(
            session_id,
            write_path.clone(),
            "created",
        ));
        tokio::pin!(write);
        tokio::select! {
            _ = allow_next_permission(&mut ui_rx) => {}
            result = &mut write => panic!("write completed before permission: {result:?}"),
        }
        write.await.expect("write");
        assert_eq!(
            tokio::fs::read_to_string(write_path)
                .await
                .expect("written"),
            "created"
        );
    }

    #[tokio::test]
    async fn local_filesystem_allows_additional_workspace_roots() {
        let primary = tempfile::tempdir().expect("primary");
        let additional = tempfile::tempdir().expect("additional");
        let session_id = SessionId::new("session-1");
        let state = RuntimeSessionState::new();
        state
            .set_active_session_with_roots(
                session_id.clone(),
                primary.path(),
                &[additional.path().to_path_buf()],
            )
            .await
            .expect("active roots");
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let filesystem = LocalFileSystem::new(state, ui_tx, DEFAULT_FS_TEXT_BYTES);
        let read_path = additional.path().join("notes.txt");
        tokio::fs::write(&read_path, "extra").await.expect("seed");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id.clone(), &read_path))
            .await
            .expect("read additional root");
        assert_eq!(read.content, "extra");

        let write_path = additional.path().join("created.txt");
        let write = filesystem.write_text_file(WriteTextFileRequest::new(
            session_id,
            write_path.clone(),
            "created",
        ));
        tokio::pin!(write);
        tokio::select! {
            _ = allow_next_permission(&mut ui_rx) => {}
            result = &mut write => panic!("write completed before permission: {result:?}"),
        }
        write.await.expect("write additional root");
        assert_eq!(
            tokio::fs::read_to_string(write_path)
                .await
                .expect("written"),
            "created"
        );
    }

    #[tokio::test]
    async fn local_filesystem_rejects_paths_outside_root() {
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let outside_file = outside.path().join("outside.txt");
        tokio::fs::write(&outside_file, "secret")
            .await
            .expect("outside file");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) = test_filesystem(root.path(), &session_id).await;

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(
                    session_id.clone(),
                    outside_file.clone()
                ))
                .await
                .is_err()
        );
        assert!(
            filesystem
                .write_text_file(WriteTextFileRequest::new(
                    session_id,
                    outside_file,
                    "overwrite"
                ))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn local_filesystem_rejects_inactive_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let active_session_id = SessionId::new("active");
        let (filesystem, _ui_rx, state) = test_filesystem(temp.path(), &active_session_id).await;
        let path = temp.path().join("notes.txt");
        tokio::fs::write(&path, "hello").await.expect("seed file");

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(SessionId::new("stale"), &path))
                .await
                .is_err()
        );

        state
            .set_active_session(SessionId::new("stale"), temp.path())
            .await
            .expect("activate stale");
        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(SessionId::new("stale"), path))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn local_filesystem_updates_root_with_active_session() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        let first_path = first.path().join("notes.txt");
        let second_path = second.path().join("notes.txt");
        tokio::fs::write(&first_path, "first")
            .await
            .expect("first file");
        tokio::fs::write(&second_path, "second")
            .await
            .expect("second file");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, state) = test_filesystem(first.path(), &session_id).await;

        assert_eq!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id.clone(), &first_path))
                .await
                .expect("read first")
                .content,
            "first"
        );

        state
            .set_active_session(session_id.clone(), second.path())
            .await
            .expect("switch root");

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id.clone(), &first_path))
                .await
                .is_err()
        );
        assert_eq!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id, &second_path))
                .await
                .expect("read second")
                .content,
            "second"
        );
    }

    #[tokio::test]
    async fn local_filesystem_uses_configured_text_limit_for_reads_and_writes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, mut ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("large.txt");
        tokio::fs::write(&path, "12345").await.expect("large file");

        assert!(
            filesystem
                .read_text_file(ReadTextFileRequest::new(session_id.clone(), &path))
                .await
                .is_err()
        );
        assert!(
            filesystem
                .write_text_file(WriteTextFileRequest::new(
                    session_id,
                    temp.path().join("new.txt"),
                    "12345",
                ))
                .await
                .is_err()
        );
        assert!(ui_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_filesystem_reads_bounded_line_range_from_large_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("large.txt");
        tokio::fs::write(&path, "long-first-line\nok\n")
            .await
            .expect("large file");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id, &path).line(2).limit(1))
            .await
            .expect("bounded read");

        assert_eq!(read.content, "ok\n");
    }

    #[tokio::test]
    async fn local_filesystem_rejects_bounded_read_after_scan_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("huge-first-line.txt");
        let mut content = vec![b'a'; DEFAULT_FS_TEXT_BYTES as usize + 1];
        content.extend_from_slice(b"\nok\n");
        tokio::fs::write(&path, content).await.expect("large file");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id, &path).line(2).limit(1))
            .await;

        assert!(read.is_err());
    }

    #[tokio::test]
    async fn local_filesystem_zero_line_limit_returns_empty_for_large_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::new("session-1");
        let (filesystem, _ui_rx, _state) =
            test_filesystem_with_limit(temp.path(), &session_id, 4).await;
        let path = temp.path().join("large.txt");
        tokio::fs::write(&path, "long-first-line\n")
            .await
            .expect("large file");

        let read = filesystem
            .read_text_file(ReadTextFileRequest::new(session_id, &path).limit(0))
            .await
            .expect("zero line read");

        assert_eq!(read.content, "");
    }

    #[test]
    fn terminal_output_buffer_truncates_on_utf8_boundary() {
        let mut buffer = TerminalOutputBuffer::new(5);
        buffer.append("éabc".as_bytes());
        assert_eq!(buffer.output, "éabc");
        assert!(!buffer.truncated);

        buffer.append("d".as_bytes());

        assert_eq!(buffer.output, "abcd");
        assert!(buffer.truncated);
        assert!(buffer.output.is_char_boundary(0));
    }

    #[tokio::test]
    async fn managed_terminal_runs_command_and_releases() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let terminals = ManagedTerminals::new(ui_tx);
        let session_id = SessionId::new("session-1");
        #[cfg(windows)]
        let script = "echo hello & exit /B 7";
        #[cfg(not(windows))]
        let script = "printf hello; exit 7";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;

        let waited = terminals
            .wait_for_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("wait terminal");
        assert_eq!(waited.exit_status.exit_code, Some(7));

        let output = terminals
            .output(TerminalOutputRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("terminal output");
        assert!(
            output.output.contains("hello"),
            "output: {:?}",
            output.output
        );
        assert_eq!(output.exit_status, Some(waited.exit_status));

        terminals
            .release(ReleaseTerminalRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("release terminal");
        assert!(
            terminals
                .output(TerminalOutputRequest::new(session_id, terminal_id))
                .await
                .is_err()
        );

        assert!(
            std::iter::from_fn(|| ui_rx.try_recv().ok()).any(|event| matches!(
                event,
                UiEvent::TerminalOutput(snapshot) if snapshot.output.contains("hello")
            )),
            "expected at least one terminal output UI event"
        );
    }

    #[tokio::test]
    async fn managed_terminal_cwd_is_limited_to_active_workspace_roots() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("session-1");
        let primary = tempfile::tempdir().expect("primary");
        let additional = tempfile::tempdir().expect("additional");
        let outside = tempfile::tempdir().expect("outside");
        let session_state = RuntimeSessionState::new();
        session_state
            .set_active_session_with_roots(
                session_id.clone(),
                primary.path(),
                &[additional.path().to_path_buf()],
            )
            .await
            .expect("active roots");
        let terminals = ManagedTerminals::with_session_state(ui_tx, session_state);

        let default_cwd = terminals
            .resolve_terminal_cwd(&CreateTerminalRequest::new(session_id.clone(), "pwd"))
            .await
            .expect("default cwd")
            .expect("cwd");
        assert_eq!(
            default_cwd,
            std::fs::canonicalize(primary.path()).expect("primary")
        );

        let additional_cwd = terminals
            .resolve_terminal_cwd(
                &CreateTerminalRequest::new(session_id.clone(), "pwd")
                    .cwd(additional.path().to_path_buf()),
            )
            .await
            .expect("additional cwd")
            .expect("cwd");
        assert_eq!(
            additional_cwd,
            std::fs::canonicalize(additional.path()).expect("additional")
        );

        assert!(
            terminals
                .resolve_terminal_cwd(
                    &CreateTerminalRequest::new(session_id, "pwd")
                        .cwd(outside.path().to_path_buf()),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn release_with_wrong_session_does_not_remove_terminal() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let terminals = ManagedTerminals::new(ui_tx);
        let session_id = SessionId::new("session-1");
        let wrong_session_id = SessionId::new("session-2");
        #[cfg(windows)]
        let script = "echo hello";
        #[cfg(not(windows))]
        let script = "printf hello";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;

        assert!(
            terminals
                .release(ReleaseTerminalRequest::new(
                    wrong_session_id,
                    terminal_id.clone(),
                ))
                .await
                .is_err()
        );

        terminals
            .wait_for_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("wait terminal");
        let output = terminals
            .output(TerminalOutputRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
            .expect("terminal should remain available");
        assert!(output.output.contains("hello"));
        terminals
            .release(ReleaseTerminalRequest::new(session_id, terminal_id))
            .await
            .expect("release with correct session");
    }

    #[tokio::test]
    async fn managed_terminals_reject_inactive_sessions_and_shutdown_session() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let session_id = SessionId::new("session-1");
        let other_session_id = SessionId::new("session-2");
        let session_state = RuntimeSessionState::new();
        let root = tempfile::tempdir().expect("root");
        session_state
            .set_active_session(session_id.clone(), root.path())
            .await
            .expect("active session");
        let terminals = ManagedTerminals::with_session_state(ui_tx, session_state.clone());
        #[cfg(windows)]
        let script = "ping -n 30 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let script = "sleep 30";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;

        session_state
            .set_active_session(other_session_id.clone(), root.path())
            .await
            .expect("switch active session");
        assert!(
            terminals
                .output(TerminalOutputRequest::new(
                    session_id.clone(),
                    terminal_id.clone(),
                ))
                .await
                .is_err()
        );

        terminals.shutdown_session(&session_id).await;
        assert!(
            terminals
                .get_terminal(&session_id, &terminal_id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_all_kills_running_terminal_commands() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let terminals = ManagedTerminals::new(ui_tx);
        let session_id = SessionId::new("session-1");
        #[cfg(windows)]
        let script = "ping -n 30 127.0.0.1 >NUL";
        #[cfg(not(windows))]
        let script = "sleep 30";
        let (command, args) = terminal_test_command(script);

        let created = terminals
            .create(
                CreateTerminalRequest::new(session_id.clone(), command)
                    .args(args)
                    .output_byte_limit(1024),
            )
            .await
            .expect("create terminal");
        let terminal_id = created.terminal_id;
        let terminal = terminals
            .get_terminal(&session_id, &terminal_id)
            .await
            .expect("terminal");

        terminals.shutdown_all().await;

        assert!(
            terminals
                .output(TerminalOutputRequest::new(session_id, terminal_id))
                .await
                .is_err(),
            "shutdown must remove terminals from the active table"
        );
        tokio::time::timeout(Duration::from_secs(5), terminal.wait_for_exit())
            .await
            .expect("terminal process should exit after shutdown")
            .expect("terminal wait should resolve");
    }

    #[test]
    fn legacy_session_modes_become_config_picker_options() {
        let mode_state = SessionModeState::new(
            "medium",
            vec![
                agent_client_protocol::schema::v1::SessionMode::new("low", "Thinking: low"),
                agent_client_protocol::schema::v1::SessionMode::new("medium", "Thinking: medium"),
            ],
        );

        let (options, targets) = session_config_from_parts(None, Some(mode_state)).expect("config");

        assert_eq!(options.len(), 1);
        assert_eq!(targets, vec![SessionConfigTarget::LegacyMode]);
        assert_eq!(options[0].name, "Thinking");
        assert_eq!(
            options[0].category,
            Some(SessionConfigOptionCategory::ThoughtLevel)
        );
        assert_eq!(current_select_value(&options[0]).as_deref(), Some("medium"));
    }

    #[test]
    fn explicit_config_options_take_precedence_over_legacy_modes() {
        let config_option = SessionConfigOption::select(
            "model",
            "Configured Model",
            "model-a",
            vec![
                agent_client_protocol::schema::v1::SessionConfigSelectOption::new(
                    "model-a", "Model A",
                ),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let legacy_mode_state = SessionModeState::new(
            "medium",
            vec![agent_client_protocol::schema::v1::SessionMode::new(
                "medium",
                "Thinking: medium",
            )],
        );

        let (options, targets) =
            session_config_from_parts(Some(vec![config_option]), Some(legacy_mode_state))
                .expect("config");

        assert_eq!(options.len(), 1);
        assert_eq!(options[0].name, "Configured Model");
        assert_eq!(
            options[0].category,
            Some(SessionConfigOptionCategory::Model)
        );
        assert_eq!(
            targets,
            vec![SessionConfigTarget::ConfigOption {
                config_id: "model".into()
            }]
        );
    }

    #[test]
    fn legacy_config_updates_current_value_locally_after_success() {
        let mode_state = SessionModeState::new(
            "medium",
            vec![
                agent_client_protocol::schema::v1::SessionMode::new("low", "Thinking: low"),
                agent_client_protocol::schema::v1::SessionMode::new("medium", "Thinking: medium"),
            ],
        );
        let (mut options, targets) =
            session_config_from_parts(None, Some(mode_state)).expect("config");

        set_current_config_value(
            &mut options,
            &targets,
            &SessionConfigTarget::LegacyMode,
            &"low".into(),
        );

        assert_eq!(current_select_value(&options[0]).as_deref(), Some("low"));
    }

    #[test]
    fn legacy_model_config_update_error_is_explicit() {
        let error = legacy_model_config_update_error();

        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert_eq!(error.message, "Invalid params");
        let data = error.data.expect("error data");
        assert_eq!(data["target"], "legacy_model");
        assert_eq!(
            data["reason"],
            "legacy session model updates are not supported by agent-client-protocol 0.14"
        );
    }

    fn current_select_value(option: &SessionConfigOption) -> Option<String> {
        match &option.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
            _ => None,
        }
    }

    /// Spawn a minimal in-process ACP agent over a duplex stream. The
    /// agent answers Initialize/NewSession/Prompt, streams one chunk back
    /// on every prompt, and reports EndTurn.
    async fn run_mock_agent(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    assert!(req.client_capabilities.terminal);
                    assert!(req.client_capabilities.fs.read_text_file);
                    assert!(req.client_capabilities.fs.write_text_file);
                    let client_info = req.client_info.expect("clientInfo");
                    assert_eq!(client_info.name, env!("CARGO_PKG_NAME"));
                    assert_eq!(client_info.version, env!("CARGO_PKG_VERSION"));
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .agent_capabilities(
                                AgentCapabilities::new()
                                    .load_session(true)
                                    .session_capabilities(
                                        SessionCapabilities::new()
                                            .fork(SessionForkCapabilities::new())
                                            .resume(SessionResumeCapabilities::new()),
                                    ),
                            ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::LoadSessionRequest,
                            responder,
                            _cx| { responder.respond(LoadSessionResponse::new()) },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ResumeSessionRequest, responder, _cx| {
                    responder.respond(ResumeSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ForkSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let old_session_id = req.session_id.clone();
                    let response = responder
                        .respond(ForkSessionResponse::new(SessionId::new("forked-session")));
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        let _ = cx.send_notification(SessionNotification::new(
                            old_session_id,
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("stale parent update")),
                            )),
                        ));
                    });
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let session_id = req.session_id.clone();
                    // Stream one chunk so the client sees a SessionUpdate
                    // before the prompt resolves.
                    let _ = cx.send_notification(SessionNotification::new(
                        session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new("ack"),
                        ))),
                    ));
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                // Keep the agent alive until the client side closes.
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_additional_directories(
        stream: tokio::io::DuplexStream,
        expected_additional_directories: Vec<PathBuf>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .additional_directories(
                                            SessionAdditionalDirectoriesCapabilities::new(),
                                        )
                                        .close(SessionCloseCapabilities::new())
                                        .fork(SessionForkCapabilities::new())
                                        .resume(SessionResumeCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let expected_additional_directories = expected_additional_directories.clone();
                    async move |req: NewSessionRequest, responder, _cx| {
                        assert_eq!(
                            req.additional_directories, expected_additional_directories,
                            "session/new should receive requested additional directories"
                        );
                        responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let expected_additional_directories = expected_additional_directories.clone();
                    async move |req: ResumeSessionRequest, responder, _cx| {
                        assert_eq!(
                            req.additional_directories, expected_additional_directories,
                            "session/resume should receive requested additional directories"
                        );
                        responder.respond(ResumeSessionResponse::new())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let expected_additional_directories = expected_additional_directories.clone();
                    async move |req: ForkSessionRequest, responder, _cx| {
                        assert_eq!(
                            req.additional_directories, expected_additional_directories,
                            "session/fork should receive requested additional directories"
                        );
                        responder
                            .respond(ForkSessionResponse::new(SessionId::new("forked-session")))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: CloseSessionRequest, responder, _cx| {
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_load_additional_directories(
        stream: tokio::io::DuplexStream,
        expected_additional_directories: Vec<PathBuf>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .additional_directories(
                                            SessionAdditionalDirectoriesCapabilities::new(),
                                        )
                                        .close(SessionCloseCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: CloseSessionRequest, responder, _cx| {
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: LoadSessionRequest, responder, _cx| {
                    assert_eq!(
                        req.additional_directories, expected_additional_directories,
                        "session/load should receive requested additional directories"
                    );
                    responder.respond(LoadSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_filesystem_requests(
        stream: tokio::io::DuplexStream,
        read_path: PathBuf,
        write_path: PathBuf,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    assert!(req.client_capabilities.fs.read_text_file);
                    assert!(req.client_capabilities.fs.write_text_file);
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    let response =
                        responder.respond(NewSessionResponse::new(SessionId::new("test-session")));
                    let read_path = read_path.clone();
                    let write_path = write_path.clone();
                    tokio::spawn(async move {
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                        let read = loop {
                            match cx
                                .send_request(
                                    ReadTextFileRequest::new(
                                        SessionId::new("test-session"),
                                        read_path.clone(),
                                    )
                                    .line(2)
                                    .limit(1),
                                )
                                .block_task()
                                .await
                            {
                                Ok(read) => break read,
                                Err(err) if tokio::time::Instant::now() < deadline => {
                                    tokio::time::sleep(Duration::from_millis(10)).await;
                                    tracing::debug!("retry filesystem read after error: {err:?}");
                                }
                                Err(err) => panic!("read text file: {err:?}"),
                            }
                        };
                        assert_eq!(read.content, "two\n");

                        cx.send_request(WriteTextFileRequest::new(
                            SessionId::new("test-session"),
                            write_path,
                            "written by agent",
                        ))
                        .block_task()
                        .await
                        .expect("write text file");
                    });
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_hanging_config(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: SetSessionConfigOptionRequest, _responder, _cx| {
                    futures::future::pending::<()>().await;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_hanging_fork(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().fork(SessionForkCapabilities::new()),
                            )),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ForkSessionRequest, _responder, _cx| {
                    futures::future::pending::<()>().await;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_cancel(
        stream: tokio::io::DuplexStream,
        cancel_hits: Arc<AtomicUsize>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let cancel_rx_for_prompt = cancel_rx.clone();
        let cancel_tx_for_notification = cancel_tx.clone();
        let cancel_hits_for_notification = cancel_hits.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            _cx| {
                    let mut cancel_rx = cancel_rx_for_prompt.clone();
                    tokio::spawn(async move {
                        while !*cancel_rx.borrow() {
                            if cancel_rx.changed().await.is_err() {
                                break;
                            }
                        }
                        let _ = responder.respond(PromptResponse::new(StopReason::Cancelled));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |_notif: agent_client_protocol::schema::v1::CancelNotification, _cx| {
                    cancel_hits_for_notification.fetch_add(1, Ordering::SeqCst);
                    let _ = cancel_tx_for_notification.send(true);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_pending_permission(
        stream: tokio::io::DuplexStream,
        permission_cancelled: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            cx| {
                    let permission_cancelled = permission_cancelled.clone();
                    tokio::spawn(async move {
                        let response = cx
                            .send_request(RequestPermissionRequest::new(
                                SessionId::new("test-session"),
                                agent_client_protocol::schema::v1::ToolCallUpdate::new(
                                    "call-1",
                                    ToolCallUpdateFields::default(),
                                ),
                                vec![PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                )],
                            ))
                            .block_task()
                            .await;
                        let stop_reason = match response {
                            Ok(resp)
                                if matches!(resp.outcome, RequestPermissionOutcome::Cancelled) =>
                            {
                                permission_cancelled.store(true, Ordering::SeqCst);
                                StopReason::Cancelled
                            }
                            _ => StopReason::EndTurn,
                        };
                        let _ = responder.respond(PromptResponse::new(stop_reason));
                    });
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_with_prompt_error(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::PromptRequest,
                            responder,
                            _cx| { responder.respond_with_internal_error("boom") },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    /// Initialize succeeds, but session/new responds with auth_required
    /// (-32000). Used to exercise the LaunchError::AuthRequired path.
    async fn run_mock_agent_session_auth_required(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V1,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond_with_error(
                        agent_client_protocol::Error::auth_required()
                            .data(serde_json::Value::String("login required".to_string())),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_auth_required_then_authenticates(stream: tokio::io::DuplexStream) {
        let authenticated = Arc::new(StdAtomicBool::new(false));
        let new_session_attempts = Arc::new(AtomicUsize::new(0));
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let authenticate_seen = authenticated.clone();
        let session_authenticated = authenticated.clone();
        let session_attempts = new_session_attempts.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::AuthenticateRequest,
                            responder,
                            _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    session_attempts.fetch_add(1, Ordering::SeqCst);
                    if session_authenticated.load(Ordering::SeqCst) {
                        responder.respond(NewSessionResponse::new(SessionId::new("test-session")))
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::auth_required()
                                .data(serde_json::Value::String("login required".to_string())),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_load_auth_required_then_authenticates(stream: tokio::io::DuplexStream) {
        let authenticated = Arc::new(StdAtomicBool::new(false));
        let load_session_attempts = Arc::new(AtomicUsize::new(0));
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let authenticate_seen = authenticated.clone();
        let session_authenticated = authenticated.clone();
        let session_attempts = load_session_attempts.clone();
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().load_session(true))
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::AuthenticateRequest,
                            responder,
                            _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: agent_client_protocol::schema::v1::LoadSessionRequest,
                            responder,
                            _cx| {
                    assert_eq!(req.session_id.to_string(), "existing-session");
                    session_attempts.fetch_add(1, Ordering::SeqCst);
                    if session_authenticated.load(Ordering::SeqCst) {
                        responder.respond(LoadSessionResponse::new())
                    } else {
                        responder.respond_with_error(
                            agent_client_protocol::Error::auth_required()
                                .data(serde_json::Value::String("login required".to_string())),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_unsupported_protocol(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(InitializeResponse::new(
                        agent_client_protocol::schema::ProtocolVersion::V0,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_inline_session_switch(
        stream: tokio::io::DuplexStream,
        close_seen: Arc<StdAtomicBool>,
        resume_seen: Arc<StdAtomicBool>,
        stale_permission_cancelled: Arc<StdAtomicBool>,
    ) {
        let close_seen_for_req = close_seen.clone();
        let resume_seen_for_req = resume_seen.clone();
        let stale_permission_cancelled_for_req = stale_permission_cancelled.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new()
                                    .close(SessionCloseCapabilities::new())
                                    .resume(SessionResumeCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("old-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: CloseSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id.to_string(), "old-session");
                    close_seen_for_req.store(true, Ordering::SeqCst);
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ResumeSessionRequest,
                            responder,
                            cx: ConnectionTo<agent_client_protocol::Client>| {
                    assert_eq!(req.session_id.to_string(), "target-session");
                    let resume_seen_for_req = resume_seen_for_req.clone();
                    let stale_permission_cancelled_for_req =
                        stale_permission_cancelled_for_req.clone();
                    resume_seen_for_req.store(true, Ordering::SeqCst);
                    let stale_permission_cx = cx.clone();
                    tokio::spawn(async move {
                        let permission_response = stale_permission_cx
                            .send_request(RequestPermissionRequest::new(
                                SessionId::new("old-session"),
                                ToolCallUpdate::new("stale-call", ToolCallUpdateFields::default()),
                                vec![PermissionOption::new(
                                    "allow",
                                    "Allow",
                                    PermissionOptionKind::AllowOnce,
                                )],
                            ))
                            .block_task()
                            .await
                            .expect("stale permission response");
                        if matches!(
                            permission_response.outcome,
                            RequestPermissionOutcome::Cancelled
                        ) {
                            stale_permission_cancelled_for_req.store(true, Ordering::SeqCst);
                        }
                    });
                    let target_session_id = req.session_id.clone();
                    let target_cx = cx.clone();
                    let response = responder.respond(ResumeSessionResponse::new());
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        let _ = target_cx.send_notification(SessionNotification::new(
                            target_session_id,
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new("target resume update")),
                            )),
                        ));
                    });
                    response
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_without_close_capability(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("old-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_without_resume_capability(
        stream: tokio::io::DuplexStream,
        close_seen: Arc<StdAtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::InitializeRequest,
                            responder,
                            _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().close(SessionCloseCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: agent_client_protocol::schema::v1::NewSessionRequest,
                            responder,
                            _cx| {
                    responder.respond(NewSessionResponse::new(SessionId::new("old-session")))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: CloseSessionRequest, responder, _cx| {
                    close_seen.store(true, Ordering::SeqCst);
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn wait_for_session_started(
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        expected_session_id: &str,
    ) {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timed out waiting for SessionStarted")
                .expect("ui event channel closed");
            if let UiEvent::SessionStarted { session_id, .. } = ev {
                assert_eq!(session_id, expected_session_id);
                return;
            }
        }
    }

    async fn wait_for_agent_message_chunk(
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        expected_text: &str,
    ) {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timed out waiting for SessionUpdate")
                .expect("ui event channel closed");
            if let UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) = ev
                && let ContentBlock::Text(text) = &chunk.content
                && text.text == expected_text
            {
                return;
            }
        }
    }

    async fn wait_for_atomic_bool(flag: &StdAtomicBool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(flag.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_prompt_turn_against_mock_agent() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        // Pull Connected + SessionStarted.
        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
            })
            .expect("send prompt");

        let mut saw_update = false;
        let mut saw_done = false;
        while !(saw_update && saw_done) {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for prompt turn")
                .expect("channel closed");
            match ev {
                UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(c)) => {
                    if let ContentBlock::Text(t) = &c.content {
                        assert_eq!(t.text, "ack");
                    }
                    saw_update = true;
                }
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::EndTurn));
                    saw_done = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_new_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_resume_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            Some("existing-session".to_string()),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "existing-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_load_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_load_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        let (responder, result_rx) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "loaded-session".to_string(),
                cwd: root.path().to_path_buf(),
                title: None,
                responder,
            })
            .expect("send load");
        assert!(matches!(
            result_rx.await.expect("load result"),
            LoadSessionResult::Switched
        ));
        wait_for_session_started(&mut ui_rx, "loaded-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_sends_additional_directories_on_fork_session() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let additional_path = std::fs::canonicalize(additional.path()).expect("canonical");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_additional_directories(
            agent_side,
            vec![additional_path.clone()],
        ));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional_path],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        cmd_tx.send(UiCommand::ForkSession).expect("send fork");
        wait_for_session_started(&mut ui_rx, "forked-session").await;
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        tokio::time::timeout(Duration::from_secs(5), client_task)
            .await
            .expect("drive_client did not finish")
            .expect("client task")
            .expect("drive_client");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_rejects_additional_directories_without_agent_capability() {
        let root = tempfile::tempdir().expect("root");
        let additional = tempfile::tempdir().expect("additional");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client_with_additional_directories(
            client_transport,
            root.path().to_path_buf(),
            vec![additional.path().to_path_buf()],
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timeout waiting for fatal")
            .expect("event");
        match ev {
            UiEvent::Fatal(msg) => assert!(
                msg.contains("sessionCapabilities.additionalDirectories"),
                "unexpected fatal: {msg}"
            ),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(5), client_task)
                .await
                .expect("drive_client did not finish")
                .expect("client task")
                .is_err()
        );
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mock_agent_can_read_and_write_text_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let read_path = temp.path().join("read.txt");
        let write_path = temp.path().join("write.txt");
        tokio::fs::write(&read_path, "one\ntwo\nthree\n")
            .await
            .expect("seed file");
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_filesystem_requests(
            agent_side,
            read_path,
            write_path.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            temp.path().to_path_buf(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "test-session").await;
        allow_next_permission(&mut ui_rx).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(content) = tokio::fs::read_to_string(&write_path).await {
                assert_eq!(content, "written by agent");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for filesystem write");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fork_session_switches_to_forked_session_id() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_initial_session = false;
        while !saw_initial_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted { session_id, .. } => {
                    assert_eq!(session_id, "test-session");
                    saw_initial_session = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");

        let mut saw_forked_session = false;
        let mut saw_forked_info = false;
        while !(saw_forked_session && saw_forked_info) {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for fork")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted { session_id, .. } => {
                    assert_eq!(session_id, "forked-session");
                    saw_forked_session = true;
                }
                UiEvent::Info(message) => {
                    assert_eq!(message, "session forked");
                    saw_forked_info = true;
                }
                UiEvent::SessionConfigOptions { .. } => {}
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }
        let stale_event = tokio::time::timeout(Duration::from_millis(200), ui_rx.recv()).await;
        assert!(
            stale_event.is_err(),
            "stale parent-session notification was forwarded: {stale_event:?}"
        );

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fork_session_without_capability_emits_warning() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_config(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted { session_id, .. } => {
                    assert_eq!(session_id, "test-session");
                    saw_session = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");

        let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timeout waiting for fork warning")
            .expect("channel closed");
        match ev {
            UiEvent::Warning(message) => {
                assert_eq!(
                    message,
                    "session fork is not supported by this agent (unstable ACP extension not advertised)"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resumed_prompt_turn_against_mock_agent() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            Some("existing-session".to_string()),
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_resumed_session = false;
        while !saw_resumed_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for resumed handshake")
                .expect("channel closed");
            match ev {
                UiEvent::SessionStarted {
                    session_id,
                    resumed,
                } => {
                    assert_eq!(session_id, "existing-session");
                    assert!(resumed);
                    saw_resumed_session = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "resume".to_string(),
                images: Vec::new(),
            })
            .expect("send prompt");

        let mut saw_done = false;
        while !saw_done {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for resumed prompt")
                .expect("channel closed");
            match ev {
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::EndTurn));
                    saw_done = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_error_emits_prompt_failed() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_prompt_error(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) | UiEvent::PromptFailed { .. } => {
                    panic!("unexpected: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
            })
            .expect("send prompt");

        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for failed prompt")
                .expect("channel closed");
            match ev {
                UiEvent::PromptFailed { message } => {
                    assert!(message.contains("prompt failed:"));
                    assert!(message.contains("boom"));
                    break;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) | UiEvent::PromptDone { .. } => {
                    panic!("unexpected: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_cancel_notification_is_forwarded() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let cancel_hits = Arc::new(AtomicUsize::new(0));
        let agent_task = tokio::spawn(run_mock_agent_with_cancel(agent_side, cancel_hits.clone()));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
            })
            .expect("send prompt");
        cmd_tx.send(UiCommand::CancelPrompt).expect("send cancel");

        let mut saw_cancelled = false;
        while !saw_cancelled {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for cancelled prompt")
                .expect("channel closed");
            match ev {
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::Cancelled));
                    saw_cancelled = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        assert_eq!(cancel_hits.load(Ordering::SeqCst), 1);

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(Duration::from_secs(2), client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_cancel_resolves_pending_permission_as_cancelled() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let permission_cancelled = Arc::new(StdAtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_with_pending_permission(
            agent_side,
            permission_cancelled.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_connected = false;
        let mut saw_session = false;
        while !(saw_connected && saw_session) {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for handshake")
                .expect("channel closed");
            match ev {
                UiEvent::Connected { .. } => saw_connected = true,
                UiEvent::SessionStarted { .. } => saw_session = true,
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "needs permission".to_string(),
                images: Vec::new(),
            })
            .expect("send prompt");

        let mut state = AppState::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for permission request")
                .expect("channel closed");
            match ev {
                UiEvent::PermissionRequest(_) => {
                    state.apply_event(ev);
                    assert!(state.has_pending_permission());
                    break;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) | UiEvent::PromptDone { .. } => {
                    panic!("unexpected before permission: {ev:?}")
                }
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::CancelPrompt).expect("send cancel");

        let mut saw_cancel_event = false;
        let mut saw_cancelled_prompt = false;
        while !(saw_cancel_event && saw_cancelled_prompt) {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for permission cancellation")
                .expect("channel closed");
            match ev {
                UiEvent::CancelPendingPermissions => {
                    state.apply_event(ev);
                    assert!(!state.has_pending_permission());
                    saw_cancel_event = true;
                }
                UiEvent::PromptDone { stop_reason, .. } => {
                    assert!(matches!(stop_reason, StopReason::Cancelled));
                    saw_cancelled_prompt = true;
                }
                UiEvent::Warning(_) | UiEvent::Fatal(_) => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        assert!(permission_cancelled.load(Ordering::SeqCst));

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(Duration::from_secs(2), client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_interrupts_hanging_config_update() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_config(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target: SessionConfigTarget::ConfigOption {
                    config_id: SessionConfigId::new("model"),
                },
                value: SessionConfigValueId::new("model-2"),
            })
            .expect("send config update");
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");

        let join = tokio::time::timeout(Duration::from_secs(2), client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_interrupts_hanging_fork() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_fork(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");
        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");

        let join = tokio::time::timeout(Duration::from_secs(2), client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_during_fork_emits_prompt_failed() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_fork(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx.send(UiCommand::ForkSession).expect("send fork");
        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
            })
            .expect("send prompt");

        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for prompt rejection")
                .expect("channel closed");
            match ev {
                UiEvent::PromptFailed { message } => {
                    assert_eq!(message, "prompt failed: session fork already in flight");
                    break;
                }
                UiEvent::Fatal(_) | UiEvent::PromptDone { .. } => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(Duration::from_secs(2), client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_during_config_update_emits_prompt_failed() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_with_hanging_config(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        cmd_tx
            .send(UiCommand::SetSessionConfigOption {
                target: SessionConfigTarget::ConfigOption {
                    config_id: SessionConfigId::new("model"),
                },
                value: SessionConfigValueId::new("model-2"),
            })
            .expect("send config update");
        cmd_tx
            .send(UiCommand::SendPrompt {
                text: "hello".to_string(),
                images: Vec::new(),
            })
            .expect("send prompt");

        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for prompt rejection")
                .expect("channel closed");
            match ev {
                UiEvent::PromptFailed { message } => {
                    assert_eq!(message, "prompt failed: config update already in flight");
                    break;
                }
                UiEvent::Fatal(_) | UiEvent::PromptDone { .. } => panic!("unexpected: {ev:?}"),
                _ => {}
            }
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        let join = tokio::time::timeout(Duration::from_secs(2), client_task)
            .await
            .expect("drive_client did not return after shutdown");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    /// Dropping the command channel must drive `drive_client` to a clean
    /// return promptly -- this is the graceful shutdown path the main
    /// binary relies on (UI exits, `cmd_tx` is dropped, the ACP task
    /// joins within the timeout instead of needing `abort()`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_returns_when_command_channel_drops() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        // Wait for the handshake so we know the loop is actually inside
        // its `recv()` waiting on commands.
        let mut saw_session = false;
        while !saw_session {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("handshake timeout")
                .expect("channel closed");
            if matches!(ev, UiEvent::SessionStarted { .. }) {
                saw_session = true;
            }
        }

        // Drop the sender side. drive_session sees `None` on its
        // `recv()` and must return; drive_client must then resolve.
        drop(cmd_tx);

        let join = tokio::time::timeout(Duration::from_secs(2), client_task)
            .await
            .expect("drive_client did not return after cmd channel drop");
        join.expect("client task panicked")
            .expect("drive_client returned error");
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_spawn_failure_as_fatal() {
        let cfg = AcpRuntimeConfig {
            command: PathBuf::from("definitely-not-a-real-mjolnir-command"),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            resume_session: None,
            env: HashMap::new(),
            agent_stderr: None,
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
        };
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let run_task = tokio::spawn(run(cfg, ui_tx, cmd_rx));

        let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timeout waiting for fatal event")
            .expect("channel closed");
        match ev {
            UiEvent::Fatal(msg) => {
                assert!(
                    msg.contains("agent command not found"),
                    "unexpected fatal: {msg}"
                );
                assert!(
                    msg.contains("hint:"),
                    "expected action hint in fatal: {msg}"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("run task did not finish");
        assert!(result.expect("run task panicked").is_err());
    }

    /// End-to-end check that a bad `--agent-stderr` path emits the right
    /// flag in the Fatal text (regression for the SpawnFailed
    /// mis-attribution we used to ship). Portable: the stderr file open
    /// fails *before* spawn touches the command, so the command path
    /// doesn't have to exist on either platform.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_blames_agent_stderr_flag_when_stderr_file_open_fails() {
        // Use a relative path whose parent doesn't exist; Rust's path
        // APIs handle forward slashes on Windows too, so create(true)
        // fails with NotFound on both Linux/macOS and Windows.
        let bad_stderr = std::env::temp_dir()
            .join("mj-bridge-cse-no-such-dir")
            .join("agent.err");
        let cfg = AcpRuntimeConfig {
            command: PathBuf::from("does-not-need-to-exist"),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            resume_session: None,
            env: HashMap::new(),
            agent_stderr: Some(bad_stderr),
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
        };
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

        let run_task = tokio::spawn(run(cfg, ui_tx, cmd_rx));

        let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timeout waiting for fatal")
            .expect("channel closed");
        match ev {
            UiEvent::Fatal(msg) => {
                assert!(
                    msg.contains("--agent-stderr"),
                    "expected --agent-stderr in fatal: {msg}"
                );
                assert!(
                    !msg.contains("--command"),
                    "must not blame --command: {msg}"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let result = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("run task did not finish");
        assert!(result.expect("run task panicked").is_err());
    }

    /// Helper: drive `run` against a launch config, drain events until a
    /// Fatal arrives or the channel closes, and assert the Fatal carries
    /// the friendly "agent process exited" wording plus a hint. Used by
    /// the two tests below that target the two distinct internal paths
    /// (wait-branch vs post-drive snapshot) which both surface the same
    /// user-visible message.
    async fn assert_run_reports_agent_exited(cfg: AcpRuntimeConfig) {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let run_task = tokio::spawn(run(cfg, ui_tx, cmd_rx));

        let mut got_fatal = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for fatal")
                .expect("channel closed");
            if let UiEvent::Fatal(msg) = ev {
                got_fatal = Some(msg);
                break;
            }
        }
        let msg = got_fatal.expect("did not receive Fatal");
        assert!(
            msg.contains("agent process exited"),
            "unexpected fatal wording: {msg}"
        );
        assert!(
            msg.contains("hint:"),
            "expected action hint in fatal: {msg}"
        );

        assert!(
            ui_rx.recv().await.is_none(),
            "expected the runtime to close the event channel after Fatal"
        );
        let result = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("run task did not finish");
        assert!(result.expect("run task panicked").is_err());
    }

    /// Build a subprocess command that starts and exits successfully
    /// without ever speaking ACP. Portable across Linux / macOS /
    /// Windows so the agent-exit tests can run everywhere.
    fn quick_exit_command() -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            (PathBuf::from("cmd"), vec!["/C".into(), "exit 0".into()])
        } else {
            (PathBuf::from("/bin/sh"), vec!["-c".into(), "exit 0".into()])
        }
    }

    /// Build a subprocess command that starts, waits long enough that
    /// `drive_result` stays pending, and then exits. We need the child
    /// to *still be alive* when the test asserts so that `child.wait()`
    /// is the branch that resolves, not the transport read.
    fn hang_then_exit_command() -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            // `ping -n 2 127.0.0.1` sleeps roughly one second on Windows
            // (one ping immediately, one after a 1-second timeout) then
            // exits. Slower than Unix's `sleep 0.3` but reliable without
            // requiring the `timeout` builtin (which is missing on some
            // SKUs and refuses to run when stdin is redirected).
            (
                PathBuf::from("cmd"),
                vec!["/C".into(), "ping 127.0.0.1 -n 2 > nul".into()],
            )
        } else {
            (
                PathBuf::from("/bin/sh"),
                // Read+discard the initialize bytes so the shell keeps
                // its stdout open while it sleeps; otherwise the child
                // could close stdout early and drive_result would race
                // to win.
                vec![
                    "-c".into(),
                    "head -c 200 >/dev/null; sleep 0.3; exit 0".into(),
                ],
            )
        }
    }

    /// Agent exits *immediately*, before mjolnir's `initialize` send can
    /// complete. With `biased; drive_result` first, the drive future is
    /// polled, gets a broken-pipe error, and returns Err quickly. The
    /// wait branch never fires; instead the post-drive `try_wait()`
    /// snapshot rescues the message wording. This nails down the
    /// "drive-Err + child-dead snapshot" path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_agent_exit_via_post_drive_snapshot() {
        let (command, args) = quick_exit_command();
        let cfg = AcpRuntimeConfig {
            command,
            args,
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            resume_session: None,
            env: HashMap::new(),
            agent_stderr: None,
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
        };
        assert_run_reports_agent_exited(cfg).await;
    }

    /// Agent hangs at `initialize` (never responds) then exits after a
    /// short sleep. Drive_result stays pending (no JSON-RPC response,
    /// pipes remain open while the child sleeps). When the child exits,
    /// `child.wait()` resolves first. This nails down the "wait-branch
    /// wins the race" path that the post-drive snapshot wouldn't reach.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_reports_agent_exit_via_wait_branch() {
        let (command, args) = hang_then_exit_command();
        let cfg = AcpRuntimeConfig {
            command,
            args,
            cwd: std::env::temp_dir(),
            additional_directories: Vec::new(),
            resume_session: None,
            env: HashMap::new(),
            agent_stderr: None,
            fs_max_text_bytes: DEFAULT_FS_TEXT_BYTES,
        };
        assert_run_reports_agent_exited(cfg).await;
    }

    #[test]
    fn npx_program_detection_accepts_bare_npx_and_windows_extension() {
        assert!(is_program_name(std::path::Path::new("npx"), "npx"));
        assert!(is_program_name(std::path::Path::new("npx.cmd"), "npx"));
        assert!(!is_program_name(
            std::path::Path::new("/usr/bin/npx"),
            "npx"
        ));
        assert!(!is_program_name(std::path::Path::new("uvx"), "npx"));
    }

    #[test]
    fn node24_archive_suffix_matches_supported_platforms() {
        let suffix = node24_archive_suffix();
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64" | "aarch64")
            | ("macos", "x86_64" | "aarch64")
            | ("windows", "x86_64" | "aarch64") => assert!(suffix.is_some()),
            _ => assert!(suffix.is_none()),
        }
    }

    #[test]
    fn node_install_failure_message_points_to_manual_install_docs() {
        let text = LaunchError::NodeInstallFailed {
            source: "network unavailable".to_string(),
        }
        .to_string();
        assert!(text.contains("npx is required"));
        assert!(text.contains("Node.js 24"));
        assert!(text.contains("https://nodejs.org/en/download"));
    }

    #[test]
    fn uvx_program_detection_accepts_bare_uvx_and_windows_extension() {
        assert!(is_program_name(std::path::Path::new("uvx"), "uvx"));
        assert!(is_program_name(std::path::Path::new("uvx.exe"), "uvx"));
        assert!(!is_program_name(
            std::path::Path::new("/usr/bin/uvx"),
            "uvx"
        ));
        assert!(!is_program_name(std::path::Path::new("npx"), "uvx"));
    }

    #[test]
    fn uv_install_failure_message_points_to_manual_install_docs() {
        let text = LaunchError::UvInstallFailed {
            source: "network unavailable".to_string(),
        }
        .to_string();
        assert!(text.contains("uvx is required"));
        assert!(text.contains("https://docs.astral.sh/uv/getting-started/installation/"));
    }

    #[test]
    fn classify_spawn_error_distinguishes_not_found_from_other_io_errors() {
        let cmd = std::path::Path::new("does-not-matter");
        let not_found =
            classify_spawn_error(cmd, std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(
            matches!(not_found, LaunchError::CommandNotFound { .. }),
            "expected CommandNotFound, got {not_found:?}"
        );

        let permission = classify_spawn_error(
            cmd,
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(
            matches!(permission, LaunchError::SpawnFailed { .. }),
            "expected SpawnFailed for permission denied, got {permission:?}"
        );
    }

    #[test]
    fn classify_session_error_routes_auth_required_separately() {
        // -32000 is the JSON-RPC code for ACP's AuthRequired.
        let auth = classify_session_error(
            agent_client_protocol::Error::auth_required()
                .data(serde_json::Value::String("login first".into())),
        );
        match auth {
            LaunchError::AuthRequired { detail } => {
                assert_eq!(detail.as_deref(), Some("login first"));
            }
            other => panic!("expected AuthRequired, got {other:?}"),
        }

        let other = classify_session_error(agent_client_protocol::Error::invalid_params());
        assert!(
            matches!(other, LaunchError::SessionCreateFailed { .. }),
            "expected SessionCreateFailed, got {other:?}"
        );
    }

    #[test]
    fn protocol_version_validation_rejects_unsupported_versions() {
        assert!(validate_protocol_version(ProtocolVersion::LATEST).is_ok());
        let err = validate_protocol_version(ProtocolVersion::V0).expect_err("unsupported version");
        match err {
            LaunchError::UnsupportedProtocolVersion { negotiated } => {
                assert_eq!(negotiated, ProtocolVersion::V0);
            }
            other => panic!("expected UnsupportedProtocolVersion, got {other:?}"),
        }
    }

    #[test]
    fn load_session_requires_advertised_capability() {
        let missing = require_load_session(&AgentCapabilities::new()).expect_err("missing");
        assert!(matches!(
            missing,
            LaunchError::UnsupportedCapability {
                capability: "loadSession"
            }
        ));

        let supported = AgentCapabilities::new().load_session(true);
        assert!(require_load_session(&supported).is_ok());
    }

    #[test]
    fn launch_error_display_includes_action_hint() {
        // Every launch error must carry an actionable next step so users
        // do not just see "acp: ..." with no remediation.
        let cases = [
            LaunchError::CommandNotFound {
                command: "anvil".into(),
            },
            LaunchError::SpawnFailed {
                command: "anvil".into(),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            },
            LaunchError::StderrFileOpen {
                path: std::path::PathBuf::from("/var/log/agent.err"),
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            },
            LaunchError::InitializeFailed {
                source: agent_client_protocol::Error::internal_error(),
            },
            LaunchError::AuthRequired {
                detail: Some("login".into()),
            },
            LaunchError::UnsupportedProtocolVersion {
                negotiated: ProtocolVersion::V0,
            },
            LaunchError::UnsupportedCapability {
                capability: "loadSession",
            },
            LaunchError::SessionCreateFailed {
                source: agent_client_protocol::Error::invalid_params(),
            },
        ];
        for case in cases {
            let text = case.to_string();
            assert!(text.contains("hint:"), "missing hint in: {text}");
        }
    }

    #[test]
    fn stderr_file_open_error_blames_the_right_flag() {
        // Regression: previously the agent-stderr file open failure was
        // routed to LaunchError::SpawnFailed with a synthesized command
        // string, so the hint told the user to check --command. It should
        // tell them to check --agent-stderr.
        let err = LaunchError::StderrFileOpen {
            path: std::path::PathBuf::from("/var/log/agent.err"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let text = err.to_string();
        assert!(
            text.contains("--agent-stderr"),
            "expected --agent-stderr in hint, got: {text}"
        );
        assert!(
            !text.contains("--command"),
            "stderr-file failure must not blame --command, got: {text}"
        );
        assert!(
            text.contains("/var/log/agent.err"),
            "expected the offending path in the error text, got: {text}"
        );
    }

    #[test]
    fn agent_exited_unexpectedly_msg_has_consistent_shape() {
        // Both the wait-branch and the post-drive snapshot funnel through
        // this formatter. Locking down the wording here prevents either
        // call site from drifting from the user-visible contract.
        let m1 = agent_exited_unexpectedly_msg("exit status 0");
        assert!(m1.starts_with("agent process exited unexpectedly:"));
        assert!(m1.contains("exit status 0"));
        assert!(m1.contains("hint: capture --agent-stderr"));

        let m2 = agent_exited_unexpectedly_msg("wait failed: broken pipe");
        assert!(m2.contains("wait failed: broken pipe"));
        assert!(m2.contains("hint: capture --agent-stderr"));
    }

    #[test]
    fn classify_initialize_error_routes_auth_required_to_authrequired() {
        // The ACP spec permits an agent to demand auth at initialize, not
        // just at session/new. Both stages should route AuthRequired to
        // the same actionable variant.
        let auth = classify_initialize_error(
            agent_client_protocol::Error::auth_required()
                .data(serde_json::Value::String("login first".into())),
        );
        match auth {
            LaunchError::AuthRequired { detail } => {
                assert_eq!(detail.as_deref(), Some("login first"));
            }
            other => panic!("expected AuthRequired, got {other:?}"),
        }

        let other = classify_initialize_error(agent_client_protocol::Error::internal_error());
        assert!(
            matches!(other, LaunchError::InitializeFailed { .. }),
            "non-auth errors must remain InitializeFailed, got {other:?}"
        );
    }

    #[test]
    fn emit_fatal_is_only_sent_once_per_runtime() {
        // Two distinct failure sites (e.g. drive_session classifies an
        // InitializeFailed, then the run() tail observes the bubbled-up
        // error) must NOT produce two Fatal events.
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let guard = Arc::new(AtomicBool::new(false));

        emit_fatal(&ui_tx, &guard, "first".to_string());
        emit_fatal(&ui_tx, &guard, "second".to_string());

        match ui_rx.try_recv().expect("missing first fatal") {
            UiEvent::Fatal(msg) => assert_eq!(msg, "first"),
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(
            ui_rx.try_recv().is_err(),
            "second emit_fatal should be suppressed by the guard"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_classifies_session_new_auth_required() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_session_auth_required(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        // Pull events until we see Fatal. We expect Connected first (init
        // succeeds), then Fatal from session/new.
        let mut got_fatal = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for fatal")
                .expect("channel closed");
            if let UiEvent::Fatal(msg) = ev {
                got_fatal = Some(msg);
                break;
            }
        }
        let msg = got_fatal.expect("did not receive Fatal");
        assert!(
            msg.contains("authentication"),
            "expected auth-required wording in fatal: {msg}"
        );
        assert!(
            msg.contains("login required"),
            "expected agent detail surfaced in fatal: {msg}"
        );
        assert!(fatal_emitted.load(Ordering::SeqCst));

        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_authenticates_and_retries_session_new() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_auth_required_then_authenticates(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        let mut got_started = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for session start")
                .expect("channel closed");
            if let UiEvent::SessionStarted {
                session_id,
                resumed,
            } = ev
            {
                got_started = Some((session_id, resumed));
                break;
            }
        }

        let (session_id, resumed) = got_started.expect("did not receive SessionStarted");
        assert_eq!(session_id, "test-session");
        assert!(!resumed);
        assert!(!fatal_emitted.load(Ordering::SeqCst));

        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_command_switches_on_existing_connection() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let close_seen = Arc::new(StdAtomicBool::new(false));
        let resume_seen = Arc::new(StdAtomicBool::new(false));
        let stale_permission_cancelled = Arc::new(StdAtomicBool::new(false));

        let agent_task = tokio::spawn(run_mock_agent_inline_session_switch(
            agent_side,
            close_seen.clone(),
            resume_seen.clone(),
            stale_permission_cancelled.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "target-session".to_string(),
                cwd: std::env::temp_dir(),
                title: Some("Target title".to_string()),
                responder,
            })
            .expect("send load session");

        assert_eq!(
            response.await.expect("load response"),
            LoadSessionResult::Switched
        );
        wait_for_session_started(&mut ui_rx, "target-session").await;
        wait_for_agent_message_chunk(&mut ui_rx, "target resume update").await;

        assert!(close_seen.load(Ordering::SeqCst));
        assert!(resume_seen.load(Ordering::SeqCst));
        wait_for_atomic_bool(&stale_permission_cancelled).await;

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_command_falls_back_without_close_capability() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_without_close_capability(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "target-session".to_string(),
                cwd: std::env::temp_dir(),
                title: None,
                responder,
            })
            .expect("send load session");

        match response.await.expect("load response") {
            LoadSessionResult::Fallback { message } => {
                assert!(message.contains("sessionCapabilities.close"));
            }
            other => panic!("expected fallback, got {other:?}"),
        }

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn load_session_command_falls_back_before_close_without_resume_or_load_capability() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let close_seen = Arc::new(StdAtomicBool::new(false));

        let agent_task = tokio::spawn(run_mock_agent_without_resume_capability(
            agent_side,
            close_seen.clone(),
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            Arc::new(AtomicBool::new(false)),
        ));

        wait_for_session_started(&mut ui_rx, "old-session").await;

        let (responder, response) = oneshot::channel();
        cmd_tx
            .send(UiCommand::LoadSession {
                session_id: "target-session".to_string(),
                cwd: std::env::temp_dir(),
                title: None,
                responder,
            })
            .expect("send load session");

        match response.await.expect("load response") {
            LoadSessionResult::Fallback { message } => {
                assert!(message.contains("sessionCapabilities.resume or loadSession"));
            }
            other => panic!("expected fallback, got {other:?}"),
        }
        assert!(!close_seen.load(Ordering::SeqCst));

        cmd_tx.send(UiCommand::Shutdown).expect("shutdown");
        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_authenticates_and_retries_session_load() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_load_auth_required_then_authenticates(
            agent_side,
        ));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            Some("existing-session".to_string()),
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        let mut got_started = None;
        for _ in 0..6 {
            let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timeout waiting for session start")
                .expect("channel closed");
            if let UiEvent::SessionStarted {
                session_id,
                resumed,
            } = ev
            {
                got_started = Some((session_id, resumed));
                break;
            }
        }

        let (session_id, resumed) = got_started.expect("did not receive SessionStarted");
        assert_eq!(session_id, "existing-session");
        assert!(resumed);
        assert!(!fatal_emitted.load(Ordering::SeqCst));

        client_task.abort();
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_client_rejects_unsupported_protocol_version() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());

        let agent_task = tokio::spawn(run_mock_agent_unsupported_protocol(agent_side));

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiEvent>();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
        let fatal_emitted = Arc::new(AtomicBool::new(false));

        let client_task = tokio::spawn(drive_client(
            client_transport,
            std::env::temp_dir(),
            None,
            ui_tx,
            cmd_rx,
            fatal_emitted.clone(),
        ));

        let ev = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timeout waiting for fatal")
            .expect("channel closed");
        match ev {
            UiEvent::Fatal(msg) => {
                assert!(msg.contains("unsupported ACP protocol version"), "{msg}");
                assert!(msg.contains("hint:"), "{msg}");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = tokio::time::timeout(Duration::from_secs(2), client_task).await;
        agent_task.abort();
    }
}
