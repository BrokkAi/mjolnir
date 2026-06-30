//! Async startup validation of ACP agents.
//!
//! At startup — in the background, before the agent picker is even on screen
//! — we validate the agents the picker shows by default (its curated +
//! favorite + default set; see [`crate::picker::spawn_startup_probes`]) so
//! the picker can show, at a glance, which agents will "just work". We
//! deliberately do *not* probe the full registry: spawning a subprocess per
//! agent is expensive, and the long tail behind "Other..." is rarely used.
//!
//! For every in-scope agent we can launch *without* triggering an install,
//! we run the ACP handshake through to `session/new` — the only point at
//! which an agent reveals its session config options (models/modes) — and
//! classify the result:
//!
//! * [`ProbeStatus::Checking`] — seeded while the probe is in flight.
//! * [`ProbeStatus::Configured`] — the agent opened a session, so we were
//!   able to read its session config options. The picker shows a green
//!   check.
//! * [`ProbeStatus::NeedsAuth`] — reachable, but `initialize` or
//!   `session/new` returned `auth_required`; the user must authenticate
//!   first. (The probe never attempts to authenticate — needing auth *is*
//!   the signal.)
//! * [`ProbeStatus::NotInstalled`] — the launcher (`uvx`/`npx`) or binary
//!   is not present locally. We never install it just to probe.
//! * [`ProbeStatus::Unknown`] — the probe ran out of time. This is *not* a
//!   failure: a cold `npx`/`uvx` first-run package fetch can legitimately
//!   outlast the budget, so we report it neutrally rather than as broken.
//! * [`ProbeStatus::Failed`] — spawn/handshake error or unsupported protocol.
//!
//! Results land in a process-global store ([`record`]/[`snapshot`]) that the
//! picker reads each frame, so probing is fully decoupled from when (or
//! whether) the picker opens.
//!
//! Opening a session is a real side effect: it creates a throwaway session.
//! The probe deletes it again when the agent advertises
//! `sessionCapabilities.delete`; agents without delete support may be left
//! with one empty session. The probe session is rooted at the process cwd
//! (`std::env::current_dir`), which is sufficient because session config
//! options are account/agent-level, not workspace-specific.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    DeleteSessionRequest, ErrorCode, InitializeRequest, NewSessionRequest,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::acp;

type AgentTransport = ByteStreams<
    tokio_util::compat::Compat<tokio::process::ChildStdin>,
    tokio_util::compat::Compat<tokio::process::ChildStdout>,
>;

struct DetachedAgentLaunch {
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: PathBuf,
    timeout: Duration,
}

struct DetachedAgentOutcomes<Missing, SpawnFailed, TimedOut> {
    missing: Missing,
    spawn_failed: SpawnFailed,
    timed_out: TimedOut,
}

/// Process-global validation results, keyed by picker `source_id`. Written
/// by the background probes, read by the picker each frame. A poisoned lock
/// degrades to "no results" rather than panicking the UI.
static RESULTS: LazyLock<Mutex<HashMap<String, ProbeStatus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record a probe result for `source_id`, overwriting any prior value.
pub fn record(source_id: String, status: ProbeStatus) {
    if let Ok(mut results) = RESULTS.lock() {
        results.insert(source_id, status);
    }
}

/// Snapshot the current results so the picker can render a frame without
/// holding the lock across the draw.
pub fn snapshot() -> HashMap<String, ProbeStatus> {
    RESULTS.lock().map(|m| m.clone()).unwrap_or_default()
}

/// Maximum agents probed concurrently. Probing spawns a subprocess (and,
/// for `npx`/`uvx` agents, may fetch the package on first run), so we cap
/// the fan-out to avoid a thundering herd at startup.
pub const PROBE_CONCURRENCY: usize = 5;

/// Per-agent probe budget. Covers spawn + any first-run package fetch +
/// the `initialize`/`session/new` round-trips. Generous because a cold
/// `npx`/`uvx` fetch can be slow; probes run in the background and update
/// the picker live, so a long-but-rare wait does not block the UI. A probe
/// that still exceeds this is reported as [`ProbeStatus::Unknown`], not a
/// failure.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of probing one agent. Keyed back to the picker row by its
/// `source_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeStatus {
    /// The probe has been queued/started but has not yet reached a verdict.
    /// Seeded by the spawner so the picker can show in-progress agents and
    /// distinguish them from agents that are out of scope (never probed).
    Checking,
    /// Spawned, completed `initialize`, and opened a session — so its
    /// session config options were retrievable.
    Configured,
    /// Reachable but `initialize`/`session/new` returned `auth_required`.
    NeedsAuth,
    /// Launcher/binary not present locally; not probed, not installed.
    NotInstalled,
    /// The probe exceeded its time budget before reaching a verdict — most
    /// often a cold `npx`/`uvx` package fetch. Indeterminate, not broken.
    Unknown,
    /// Could not validate (spawn failure, handshake error, or unsupported
    /// protocol version). Carries a short reason for logs.
    Failed(String),
}

/// Probe a single agent given its resolved launch command. Never installs
/// anything: if the launcher/binary is missing this returns
/// [`ProbeStatus::NotInstalled`]. `cwd` is the directory the probe session
/// is rooted at.
pub async fn probe_agent(
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: PathBuf,
    timeout: Duration,
) -> ProbeStatus {
    run_detached_agent_session(
        DetachedAgentLaunch {
            program,
            args,
            env,
            cwd,
            timeout,
        },
        DetachedAgentOutcomes {
            missing: || ProbeStatus::NotInstalled,
            spawn_failed: ProbeStatus::Failed,
            timed_out: || ProbeStatus::Unknown,
        },
        session_probe,
    )
    .await
}

async fn run_detached_agent_session<R, Missing, SpawnFailed, TimedOut, Handler, Fut>(
    launch: DetachedAgentLaunch,
    outcomes: DetachedAgentOutcomes<Missing, SpawnFailed, TimedOut>,
    handler: Handler,
) -> R
where
    Missing: FnOnce() -> R,
    SpawnFailed: FnOnce(String) -> R,
    TimedOut: FnOnce() -> R,
    Handler: FnOnce(AgentTransport, PathBuf) -> Fut,
    Fut: Future<Output = R>,
{
    let DetachedAgentLaunch {
        program,
        args,
        env,
        cwd,
        timeout,
    } = launch;
    let DetachedAgentOutcomes {
        missing,
        spawn_failed,
        timed_out,
    } = outcomes;
    let Some(prepared) = acp::resolve_agent_command_no_install(&program, &env) else {
        return missing();
    };

    let (mut child, child_stdin, child_stdout) = match acp::spawn_agent(
        &prepared.command,
        &args,
        &prepared.env,
        None,
        acp::SpawnIsolation::DetachedSession,
    ) {
        Ok(spawned) => spawned,
        Err(e) => return spawn_failed(format!("spawn failed: {e}")),
    };
    let agent_pid = child.id();
    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

    let result = match tokio::time::timeout(timeout, handler(transport, cwd)).await {
        Ok(result) => result,
        Err(_) => timed_out(),
    };

    acp::kill_agent_tree(&mut child, agent_pid).await;
    result
}

/// Drive `initialize` then `session/new` over `transport` and classify the
/// result. Reaching `session/new` is what proves we can read the agent's
/// session config options. The throwaway session is deleted again when the
/// agent advertises `sessionCapabilities.delete`.
async fn session_probe<T>(transport: T, cwd: PathBuf) -> ProbeStatus
where
    T: ConnectTo<Client>,
{
    let result: std::result::Result<ProbeStatus, agent_client_protocol::Error> = Client
        .builder()
        .connect_with(transport, move |conn: ConnectionTo<Agent>| async move {
            let init_req = InitializeRequest::new(ProtocolVersion::V1)
                .client_info(acp::client_implementation());
            let init_resp = match conn.send_request(init_req).block_task().await {
                Ok(resp) => resp,
                Err(err) if err.code == ErrorCode::AuthRequired => {
                    return Ok(ProbeStatus::NeedsAuth);
                }
                Err(err) => return Ok(ProbeStatus::Failed(format!("initialize failed: {err}"))),
            };
            if init_resp.protocol_version != ProtocolVersion::LATEST {
                return Ok(ProbeStatus::Failed(format!(
                    "unsupported ACP protocol version {}",
                    init_resp.protocol_version
                )));
            }

            // `session/new` is the first (and only) point an agent returns
            // its session config options, so a successful call is exactly
            // the "can we get the config options" signal we want.
            let session = match conn
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await
            {
                Ok(session) => session,
                Err(err) if err.code == ErrorCode::AuthRequired => {
                    return Ok(ProbeStatus::NeedsAuth);
                }
                Err(err) => return Ok(ProbeStatus::Failed(format!("session/new failed: {err}"))),
            };

            // Best-effort cleanup so the probe does not litter the agent's
            // session list. Only attempted when delete is advertised.
            if init_resp
                .agent_capabilities
                .session_capabilities
                .delete
                .is_some()
            {
                let _ = conn
                    .send_request(DeleteSessionRequest::new(session.session_id.clone()))
                    .block_task()
                    .await;
            }

            Ok(ProbeStatus::Configured)
        })
        .await;
    result.unwrap_or_else(|e| ProbeStatus::Failed(format!("connection error: {e}")))
}

/// One model an agent exposes as a selectable session config value.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelOption {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// Launch the agent, open a session, and return its Model-category config
/// options (the selectable models, with their `value`/`name`/`description`).
/// Used by `mj dump-models` to read each agent's real model list. Never
/// installs; kills the subprocess afterward. `Err` carries a short reason.
pub async fn session_models(
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: PathBuf,
    timeout: Duration,
) -> std::result::Result<Vec<ModelOption>, String> {
    run_detached_agent_session(
        DetachedAgentLaunch {
            program,
            args,
            env,
            cwd,
            timeout,
        },
        DetachedAgentOutcomes {
            missing: || Err("not installed".to_string()),
            spawn_failed: Err,
            timed_out: || Err("timed out".to_string()),
        },
        session_model_options,
    )
    .await
}

/// Drive `initialize` + `session/new` and extract the Model-category options.
async fn session_model_options<T>(
    transport: T,
    cwd: PathBuf,
) -> std::result::Result<Vec<ModelOption>, String>
where
    T: ConnectTo<Client>,
{
    let result: std::result::Result<
        std::result::Result<Vec<ModelOption>, String>,
        agent_client_protocol::Error,
    > = Client
        .builder()
        .connect_with(transport, move |conn: ConnectionTo<Agent>| async move {
            let init_req = InitializeRequest::new(ProtocolVersion::V1)
                .client_info(acp::client_implementation());
            let init_resp = match conn.send_request(init_req).block_task().await {
                Ok(resp) => resp,
                Err(err) if err.code == ErrorCode::AuthRequired => {
                    return Ok(Err("needs auth".to_string()));
                }
                Err(err) => return Ok(Err(format!("initialize failed: {err}"))),
            };
            if init_resp.protocol_version != ProtocolVersion::LATEST {
                return Ok(Err(format!(
                    "unsupported protocol {}",
                    init_resp.protocol_version
                )));
            }

            let session = match conn
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await
            {
                Ok(session) => session,
                Err(err) if err.code == ErrorCode::AuthRequired => {
                    return Ok(Err("needs auth".to_string()));
                }
                Err(err) => return Ok(Err(format!("session/new failed: {err}"))),
            };

            let models = session
                .config_options
                .unwrap_or_default()
                .iter()
                .filter(|opt| crate::app::is_model_config_option(opt))
                .filter_map(crate::app::config_option_choices)
                .flatten()
                .map(|choice| ModelOption {
                    value: choice.value.to_string(),
                    name: choice.name,
                    description: choice.description,
                })
                .collect();

            if init_resp
                .agent_capabilities
                .session_capabilities
                .delete
                .is_some()
            {
                let _ = conn
                    .send_request(DeleteSessionRequest::new(session.session_id.clone()))
                    .block_task()
                    .await;
            }

            Ok(Ok(models))
        })
        .await;
    result.unwrap_or_else(|e| Err(format!("connection error: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_program_is_not_installed() {
        let status = probe_agent(
            PathBuf::from("definitely-not-a-real-agent-binary-xyz"),
            vec![],
            HashMap::new(),
            PathBuf::from("."),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(status, ProbeStatus::NotInstalled);
    }
}
