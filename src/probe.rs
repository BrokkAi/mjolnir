//! One-shot ACP adapter probing for the model-first Council catalog.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
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

/// Maximum adapters probed concurrently.
pub const PROBE_CONCURRENCY: usize = 5;

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

/// One model an agent exposes as a selectable session config value.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelOption {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// ACP capabilities needed by the model-first adapter catalog.
#[derive(Debug, Clone)]
pub struct AdapterCapabilities {
    pub http_mcp: bool,
    pub models: Vec<ModelOption>,
}

/// Launch an ACP adapter once and capture both its initialize capabilities and
/// the model choices returned by `session/new`.
pub async fn adapter_capabilities(
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: PathBuf,
    timeout: Duration,
) -> std::result::Result<AdapterCapabilities, String> {
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
        session_adapter_capabilities,
    )
    .await
}

async fn session_adapter_capabilities<T>(
    transport: T,
    cwd: PathBuf,
) -> std::result::Result<AdapterCapabilities, String>
where
    T: ConnectTo<Client>,
{
    let result: std::result::Result<
        std::result::Result<AdapterCapabilities, String>,
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
                .filter(|option| crate::app::is_model_config_option(option))
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
                    .send_request(DeleteSessionRequest::new(session.session_id))
                    .block_task()
                    .await;
            }
            Ok(Ok(AdapterCapabilities {
                http_mcp: init_resp.agent_capabilities.mcp_capabilities.http,
                models,
            }))
        })
        .await;
    result.unwrap_or_else(|error| Err(format!("connection error: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_program_is_reported_without_installing() {
        let error = adapter_capabilities(
            PathBuf::from("definitely-not-a-real-agent-binary-xyz"),
            vec![],
            HashMap::new(),
            PathBuf::from("."),
            Duration::from_secs(1),
        )
        .await
        .expect_err("missing adapter");
        assert_eq!(error, "not installed");
    }
}
