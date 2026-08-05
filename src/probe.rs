//! One-shot ACP adapter probing for the model-first catalog.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    DeleteSessionRequest, ErrorCode, InitializeRequest, NewSessionRequest, SessionConfigOption,
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

    if let Err(error) = acp::kill_agent_tree(&mut child, agent_pid).await {
        tracing::warn!("reap detached ACP probe: {error:#}");
    }
    result
}

/// One model an agent exposes as a selectable session config value.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    pub session_config: Vec<SessionConfigOption>,
    /// True when `session_config` came from an actual `session/new` response.
    pub session_config_known: bool,
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
            let session_config = session.config_options.unwrap_or_default();
            let models = session_config
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
                session_config,
                session_config_known: true,
            }))
        })
        .await;
    result.unwrap_or_else(|error| Err(format!("connection error: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Agent as AgentRole;
    use agent_client_protocol::schema::v1::{
        AgentCapabilities, DeleteSessionResponse, InitializeResponse, McpCapabilities,
        NewSessionResponse, SessionCapabilities, SessionConfigOptionCategory,
        SessionConfigSelectOption, SessionDeleteCapabilities, SessionId,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{duplex, split};

    fn client_transport(stream: tokio::io::DuplexStream) -> impl ConnectTo<Client> {
        let (read, write) = split(stream);
        ByteStreams::new(write.compat_write(), read.compat())
    }

    async fn run_successful_agent(stream: tokio::io::DuplexStream, delete_seen: Arc<AtomicBool>) {
        let (read, write) = split(stream);
        let transport = ByteStreams::new(write.compat_write(), read.compat());
        let model = SessionConfigOption::select(
            "model",
            "Model",
            "sonnet",
            vec![
                SessionConfigSelectOption::new("sonnet", "Sonnet").description("balanced model"),
                SessionConfigSelectOption::new("opus", "Opus"),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let effort = SessionConfigOption::select(
            "effort",
            "Effort",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel);

        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new()
                                .mcp_capabilities(McpCapabilities::new().http(true))
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .delete(SessionDeleteCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: NewSessionRequest, responder, _cx| {
                    assert_eq!(req.cwd, PathBuf::from("/workspace"));
                    responder.respond(
                        NewSessionResponse::new(SessionId::new("probe-session"))
                            .config_options(vec![model.clone(), effort.clone()]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DeleteSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id, SessionId::new("probe-session"));
                    delete_seen.store(true, Ordering::SeqCst);
                    responder.respond(DeleteSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_unsupported_agent(stream: tokio::io::DuplexStream) {
        let (read, write) = split(stream);
        let transport = ByteStreams::new(write.compat_write(), read.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(InitializeResponse::new(ProtocolVersion::V0))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_auth_required_agent(stream: tokio::io::DuplexStream, during_session: bool) {
        let (read, write) = split(stream);
        let transport = ByteStreams::new(write.compat_write(), read.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    if during_session {
                        responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                    } else {
                        responder.respond_with_error(agent_client_protocol::Error::auth_required())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: NewSessionRequest, responder, _cx| {
                    responder.respond_with_error(agent_client_protocol::Error::auth_required())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

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

    #[tokio::test]
    async fn successful_probe_collects_models_capabilities_and_deletes_session() {
        let (client_side, agent_side) = duplex(64 * 1024);
        let delete_seen = Arc::new(AtomicBool::new(false));
        let agent_task = tokio::spawn(run_successful_agent(agent_side, delete_seen.clone()));

        let capabilities = session_adapter_capabilities(
            client_transport(client_side),
            PathBuf::from("/workspace"),
        )
        .await
        .expect("probe succeeds");

        assert!(capabilities.http_mcp);
        assert!(capabilities.session_config_known);
        assert_eq!(capabilities.session_config.len(), 2);
        assert_eq!(capabilities.models.len(), 2);
        assert_eq!(capabilities.models[0].value, "sonnet");
        assert_eq!(capabilities.models[0].name, "Sonnet");
        assert_eq!(
            capabilities.models[0].description.as_deref(),
            Some("balanced model")
        );
        assert_eq!(capabilities.models[1].value, "opus");
        assert!(delete_seen.load(Ordering::SeqCst));
        agent_task.abort();
    }

    #[tokio::test]
    async fn protocol_and_auth_failures_are_classified() {
        let (client_side, agent_side) = duplex(64 * 1024);
        let agent_task = tokio::spawn(run_unsupported_agent(agent_side));
        let error = session_adapter_capabilities(client_transport(client_side), PathBuf::from("."))
            .await
            .expect_err("unsupported protocol");
        assert!(error.contains("unsupported protocol"), "{error}");
        agent_task.abort();

        for (during_session, expected) in [(false, "needs auth"), (true, "needs auth")] {
            let (client_side, agent_side) = duplex(64 * 1024);
            let agent_task = tokio::spawn(run_auth_required_agent(agent_side, during_session));
            let error =
                session_adapter_capabilities(client_transport(client_side), PathBuf::from("."))
                    .await
                    .expect_err("authentication required");
            assert_eq!(error, expected);
            agent_task.abort();
        }
    }

    #[tokio::test]
    async fn closed_transport_is_reported_as_connection_error() {
        let (client_side, agent_side) = duplex(1024);
        drop(agent_side);

        let error = session_adapter_capabilities(client_transport(client_side), PathBuf::from("."))
            .await
            .expect_err("closed transport");

        assert!(error.starts_with("connection error:"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detached_runner_reports_spawn_timeout_and_handler_outcomes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let bad_program = dir.path().join("bad-agent");
        std::fs::write(&bad_program, "#!/definitely/missing/interpreter\n").unwrap();
        let mut permissions = std::fs::metadata(&bad_program).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&bad_program, permissions).unwrap();

        let result = run_detached_agent_session(
            DetachedAgentLaunch {
                program: bad_program,
                args: Vec::new(),
                env: HashMap::new(),
                cwd: dir.path().to_path_buf(),
                timeout: Duration::from_secs(1),
            },
            DetachedAgentOutcomes {
                missing: || "missing".to_string(),
                spawn_failed: |error| error,
                timed_out: || "timeout".to_string(),
            },
            |_transport, _cwd| async { "handled".to_string() },
        )
        .await;
        assert!(result.starts_with("spawn failed:"), "{result}");

        let shell_launch = |timeout| DetachedAgentLaunch {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "sleep 30".to_string()],
            env: HashMap::new(),
            cwd: dir.path().to_path_buf(),
            timeout,
        };
        let result = run_detached_agent_session(
            shell_launch(Duration::from_secs(1)),
            DetachedAgentOutcomes {
                missing: || "missing".to_string(),
                spawn_failed: |error| error,
                timed_out: || "timeout".to_string(),
            },
            |_transport, cwd| async move { cwd.display().to_string() },
        )
        .await;
        assert_eq!(result, dir.path().display().to_string());

        let result = run_detached_agent_session(
            shell_launch(Duration::from_millis(10)),
            DetachedAgentOutcomes {
                missing: || "missing".to_string(),
                spawn_failed: |error| error,
                timed_out: || "timeout".to_string(),
            },
            |_transport, _cwd| async move { futures::future::pending::<String>().await },
        )
        .await;
        assert_eq!(result, "timeout");
    }
}
