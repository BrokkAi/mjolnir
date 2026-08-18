//! Session management: listing and resuming ACP sessions.
//!
//! Provides both headless listing (`mj resume --list`) and interactive
//! session picking (`mj resume` without arguments). Sessions are listed
//! by spawning the agent, initializing ACP, calling `session/list`, and
//! collecting results before entering the TUI.

use std::path::{Path, PathBuf};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, AuthMethod, AuthenticateRequest, DeleteSessionRequest, ErrorCode,
    Implementation, InitializeRequest, ListSessionsRequest, SessionInfo,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo};
use anyhow::{Context, Result};
use serde::Serialize;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::acp;
use crate::config::SelectedAgent;

#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub adapter_source_id: Option<String>,
    pub model: Option<String>,
    pub delete_supported: bool,
}

impl From<SessionInfo> for SessionEntry {
    fn from(info: SessionInfo) -> Self {
        Self {
            session_id: info.session_id.to_string(),
            cwd: info.cwd,
            title: info.title,
            updated_at: info.updated_at,
            adapter_source_id: None,
            model: None,
            delete_supported: false,
        }
    }
}

/// Serializable session info for `mj resume --list --format json`.
#[derive(Debug, Serialize)]
pub struct SessionEntryJson {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl From<&SessionEntry> for SessionEntryJson {
    fn from(e: &SessionEntry) -> Self {
        Self {
            session_id: e.session_id.clone(),
            cwd: e.cwd.display().to_string(),
            title: e.title.clone(),
            updated_at: e.updated_at.clone(),
            adapter: e.adapter_source_id.clone(),
            model: e.model.clone(),
        }
    }
}

/// Sessions and related capabilities advertised by the agent.
#[derive(Debug, Clone)]
pub struct SessionListResult {
    pub sessions: Vec<SessionEntry>,
    pub delete_supported: bool,
}

/// Outcome of the interactive session picker.
#[derive(Debug)]
pub enum ResumeOutcome {
    /// User selected a session to resume.
    Selected(SessionEntry),
    /// User confirmed a request to delete a session.
    DeleteRequested(SessionEntry),
    /// User cancelled with Esc.
    Cancelled,
}

/// List sessions from the configured agent without entering the TUI.
pub async fn list_sessions(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
) -> Result<Vec<SessionEntry>> {
    Ok(list_sessions_with_capabilities(agent, cwd, agent_stderr)
        .await?
        .sessions)
}

/// List sessions and return the session management capabilities advertised by the agent.
pub async fn list_sessions_with_capabilities(
    agent: &SelectedAgent,
    cwd: PathBuf,
    agent_stderr: Option<&Path>,
) -> Result<SessionListResult> {
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    if crate::claude_token::is_claude_invocation(None, &agent.args) {
        crate::claude_token::ensure_fresh_before_spawn(cwd.clone(), &agent.env).await;
    }
    let prepared = acp::prepare_agent_command_for_spawn(&agent.program, &agent.env, &ui_tx)
        .await
        .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
        .context("prepare agent for session listing")?;

    let (mut child, child_stdin, child_stdout) = acp::spawn_agent(
        &prepared.command,
        &agent.args,
        &prepared.env,
        agent_stderr,
        acp::SpawnIsolation::ProcessGroup,
    )
    .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
    .context("spawn agent for session listing")?;
    let agent_pid = child.id();

    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());

    let sessions = list_sessions_via_transport(transport, cwd).await;

    acp::kill_agent_tree(&mut child, agent_pid)
        .await
        .context("reap agent after session listing")?;

    sessions
}

/// Delete a session through the configured agent.
pub async fn delete_session(
    agent: &SelectedAgent,
    session_id: String,
    agent_stderr: Option<&Path>,
) -> Result<()> {
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    if crate::claude_token::is_claude_invocation(None, &agent.args) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        crate::claude_token::ensure_fresh_before_spawn(cwd, &agent.env).await;
    }
    let prepared = acp::prepare_agent_command_for_spawn(&agent.program, &agent.env, &ui_tx)
        .await
        .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
        .context("prepare agent for session deletion")?;

    let (mut child, child_stdin, child_stdout) = acp::spawn_agent(
        &prepared.command,
        &agent.args,
        &prepared.env,
        agent_stderr,
        acp::SpawnIsolation::ProcessGroup,
    )
    .map_err(|launch_err| anyhow::anyhow!("{launch_err}"))
    .context("spawn agent for session deletion")?;
    let agent_pid = child.id();

    let transport = ByteStreams::new(child_stdin.compat_write(), child_stdout.compat());
    let result = delete_session_via_transport(transport, session_id).await;

    acp::kill_agent_tree(&mut child, agent_pid)
        .await
        .context("reap agent after session deletion")?;

    result
}

/// Drive the ACP client to list sessions over an existing transport.
async fn list_sessions_via_transport<T>(transport: T, cwd: PathBuf) -> Result<SessionListResult>
where
    T: ConnectTo<Client>,
{
    let result = Client
        .builder()
        .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
            // Initialize handshake.
            let init_req =
                InitializeRequest::new(ProtocolVersion::V1).client_info(client_implementation());
            let init_resp = conn
                .send_request(init_req)
                .block_task()
                .await
                .context("initialize for session listing")?;
            validate_protocol_version(init_resp.protocol_version)?;
            require_session_list(&init_resp.agent_capabilities)?;
            let delete_supported = session_delete_supported(&init_resp.agent_capabilities);

            // Collect all pages of sessions.
            let mut all_sessions: Vec<SessionEntry> = Vec::new();
            let mut cursor: Option<String> = None;
            let mut attempted_auth = false;
            loop {
                let mut list_req = ListSessionsRequest::new().cwd(cwd.clone());
                list_req.cursor = cursor.clone();
                let resp = match conn.send_request(list_req.clone()).block_task().await {
                    Ok(resp) => resp,
                    Err(err) if is_auth_required(&err) && !attempted_auth => {
                        authenticate_with_first_method(&conn, &init_resp.auth_methods).await?;
                        attempted_auth = true;
                        conn.send_request(list_req).block_task().await?
                    }
                    Err(err) => return Err(err),
                };
                all_sessions.extend(resp.sessions.into_iter().map(SessionEntry::from));
                match resp.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }

            for session in &mut all_sessions {
                session.delete_supported = delete_supported;
            }
            Ok(SessionListResult {
                sessions: all_sessions,
                delete_supported,
            })
        })
        .await;

    result.context("ACP client error during session listing")
}

async fn delete_session_via_transport<T>(transport: T, session_id: String) -> Result<()>
where
    T: ConnectTo<Client>,
{
    let result = Client
        .builder()
        .connect_with(transport, |conn: ConnectionTo<Agent>| async move {
            let init_req =
                InitializeRequest::new(ProtocolVersion::V1).client_info(client_implementation());
            let init_resp = conn
                .send_request(init_req)
                .block_task()
                .await
                .context("initialize for session deletion")?;
            validate_protocol_version(init_resp.protocol_version)?;
            require_session_delete(&init_resp.agent_capabilities)?;

            let delete_req = DeleteSessionRequest::new(session_id);
            match conn.send_request(delete_req.clone()).block_task().await {
                Ok(_) => Ok(()),
                Err(err) if is_auth_required(&err) => {
                    authenticate_with_first_method(&conn, &init_resp.auth_methods).await?;
                    conn.send_request(delete_req).block_task().await?;
                    Ok(())
                }
                Err(err) => Err(err),
            }
        })
        .await;

    result.context("ACP client error during session deletion")
}

async fn authenticate_with_first_method(
    conn: &ConnectionTo<Agent>,
    auth_methods: &[AuthMethod],
) -> std::result::Result<(), agent_client_protocol::Error> {
    let Some(method) = auth_methods.first() else {
        return Err(
            agent_client_protocol::Error::auth_required().data(serde_json::Value::String(
                "agent requires authentication but did not advertise any ACP auth methods"
                    .to_string(),
            )),
        );
    };
    conn.send_request(AuthenticateRequest::new(method.id().clone()))
        .block_task()
        .await?;
    Ok(())
}

fn client_implementation() -> Implementation {
    Implementation::new("brokk-mjolnir", env!("CARGO_PKG_VERSION")).title("Mjolnir")
}

fn is_auth_required(err: &agent_client_protocol::Error) -> bool {
    err.code == ErrorCode::AuthRequired
}

fn validate_protocol_version(negotiated: ProtocolVersion) -> Result<()> {
    if negotiated == ProtocolVersion::LATEST {
        Ok(())
    } else {
        anyhow::bail!(
            "agent negotiated unsupported ACP protocol version {negotiated}; mjolnir supports ACP {}",
            ProtocolVersion::LATEST
        )
    }
}

fn require_session_list(capabilities: &AgentCapabilities) -> Result<()> {
    if capabilities.session_capabilities.list.is_some() {
        Ok(())
    } else {
        anyhow::bail!("agent does not advertise ACP capability sessionCapabilities.list")
    }
}

fn session_delete_supported(capabilities: &AgentCapabilities) -> bool {
    capabilities.session_capabilities.delete.is_some()
}

fn require_session_delete(capabilities: &AgentCapabilities) -> Result<()> {
    if session_delete_supported(capabilities) {
        Ok(())
    } else {
        anyhow::bail!("agent does not advertise ACP capability sessionCapabilities.delete")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Agent as AgentRole;
    use agent_client_protocol::schema::v1::{
        AuthMethod, AuthMethodAgent, AuthenticateResponse, DeleteSessionResponse,
        InitializeResponse, ListSessionsResponse, SessionCapabilities, SessionDeleteCapabilities,
        SessionId, SessionListCapabilities,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::split;

    #[test]
    fn session_entry_json_serializes() {
        let entry = SessionEntry {
            session_id: "sess-abc".into(),
            cwd: PathBuf::from("/home/user/project"),
            title: Some("My session".into()),
            updated_at: None,
            adapter_source_id: Some("codex-acp".into()),
            model: Some("gpt-test".into()),
            delete_supported: true,
        };
        let json = SessionEntryJson::from(&entry);
        let serialized = serde_json::to_string(&json).unwrap();
        assert!(serialized.contains("sess-abc"));
        assert!(serialized.contains("My session"));
        assert!(!serialized.contains("updated_at"));
    }

    #[test]
    fn session_listing_rejects_unsupported_protocol_version() {
        let err = validate_protocol_version(ProtocolVersion::V0).expect_err("unsupported");
        assert!(err.to_string().contains("unsupported ACP protocol version"));
        assert!(validate_protocol_version(ProtocolVersion::LATEST).is_ok());
    }

    #[test]
    fn session_listing_requires_list_capability() {
        let err = require_session_list(&AgentCapabilities::new()).expect_err("missing");
        assert!(err.to_string().contains("sessionCapabilities.list"));

        let supported = AgentCapabilities::new()
            .session_capabilities(SessionCapabilities::new().list(SessionListCapabilities::new()));
        assert!(require_session_list(&supported).is_ok());
    }

    #[test]
    fn session_deletion_requires_delete_capability() {
        let err = require_session_delete(&AgentCapabilities::new()).expect_err("missing");
        assert!(err.to_string().contains("sessionCapabilities.delete"));

        let supported = AgentCapabilities::new().session_capabilities(
            SessionCapabilities::new().delete(SessionDeleteCapabilities::new()),
        );
        assert!(require_session_delete(&supported).is_ok());
    }

    async fn run_mock_agent_list_auth_required_then_authenticates(stream: tokio::io::DuplexStream) {
        let authenticated = Arc::new(AtomicBool::new(false));
        let authenticate_seen = authenticated.clone();
        let list_authenticated = authenticated.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |req: InitializeRequest, responder, _cx| {
                    let client_info = req.client_info.expect("clientInfo");
                    assert_eq!(client_info.name, "brokk-mjolnir");
                    assert_eq!(client_info.version, env!("CARGO_PKG_VERSION"));
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().list(SessionListCapabilities::new()),
                            ))
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: AuthenticateRequest, responder, _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ListSessionsRequest, responder, _cx| {
                    if list_authenticated.load(Ordering::SeqCst) {
                        responder.respond(ListSessionsResponse::new(vec![SessionInfo::new(
                            SessionId::new("listed-session"),
                            PathBuf::from("/tmp"),
                        )]))
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

    async fn run_mock_agent_list_empty_cursor_then_second_page(
        stream: tokio::io::DuplexStream,
        seen_empty_cursor: Arc<AtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new().list(SessionListCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ListSessionsRequest, responder, _cx| {
                    if req.cursor.is_none() {
                        responder.respond(
                            ListSessionsResponse::new(vec![SessionInfo::new(
                                SessionId::new("first-page"),
                                PathBuf::from("/tmp"),
                            )])
                            .next_cursor("".to_string()),
                        )
                    } else {
                        assert_eq!(req.cursor.as_deref(), Some(""));
                        seen_empty_cursor.store(true, Ordering::SeqCst);
                        responder.respond(ListSessionsResponse::new(vec![SessionInfo::new(
                            SessionId::new("second-page"),
                            PathBuf::from("/tmp"),
                        )]))
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

    async fn run_mock_agent_list_with_delete_capability(stream: tokio::io::DuplexStream) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .delete(SessionDeleteCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ListSessionsRequest, responder, _cx| {
                    responder.respond(ListSessionsResponse::new(vec![SessionInfo::new(
                        SessionId::new("delete-capable-session"),
                        PathBuf::from("/tmp"),
                    )]))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |_cx| async move {
                futures::future::pending::<()>().await;
                Ok(())
            })
            .await;
    }

    async fn run_mock_agent_delete_session(
        stream: tokio::io::DuplexStream,
        delete_seen: Arc<AtomicBool>,
    ) {
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(
                            AgentCapabilities::new().session_capabilities(
                                SessionCapabilities::new()
                                    .list(SessionListCapabilities::new())
                                    .delete(SessionDeleteCapabilities::new()),
                            ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DeleteSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id.to_string(), "delete-me");
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

    async fn run_mock_agent_delete_auth_required_then_authenticates(
        stream: tokio::io::DuplexStream,
    ) {
        let authenticated = Arc::new(AtomicBool::new(false));
        let authenticate_seen = authenticated.clone();
        let delete_authenticated = authenticated.clone();
        let (r, w) = split(stream);
        let transport = ByteStreams::new(w.compat_write(), r.compat());
        let _ = AgentRole
            .builder()
            .on_receive_request(
                async move |_req: InitializeRequest, responder, _cx| {
                    responder.respond(
                        InitializeResponse::new(ProtocolVersion::V1)
                            .agent_capabilities(
                                AgentCapabilities::new().session_capabilities(
                                    SessionCapabilities::new()
                                        .list(SessionListCapabilities::new())
                                        .delete(SessionDeleteCapabilities::new()),
                                ),
                            )
                            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                                "agent-auth",
                                "Agent Auth",
                            ))]),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: AuthenticateRequest, responder, _cx| {
                    assert_eq!(req.method_id.to_string(), "agent-auth");
                    authenticate_seen.store(true, Ordering::SeqCst);
                    responder.respond(AuthenticateResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |req: DeleteSessionRequest, responder, _cx| {
                    assert_eq!(req.session_id.to_string(), "delete-me");
                    if delete_authenticated.load(Ordering::SeqCst) {
                        responder.respond(DeleteSessionResponse::new())
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_listing_authenticates_and_retries_list() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_list_auth_required_then_authenticates(
            agent_side,
        ));

        let listing = list_sessions_via_transport(client_transport, PathBuf::from("/tmp"))
            .await
            .expect("session listing should authenticate and retry");

        assert_eq!(listing.sessions.len(), 1);
        assert_eq!(listing.sessions[0].session_id, "listed-session");
        assert!(!listing.delete_supported);

        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_listing_treats_empty_cursor_as_opaque() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let seen_empty_cursor = Arc::new(AtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_list_empty_cursor_then_second_page(
            agent_side,
            seen_empty_cursor.clone(),
        ));

        let listing = list_sessions_via_transport(client_transport, PathBuf::from("/tmp"))
            .await
            .expect("session listing should request the empty cursor page");

        assert!(seen_empty_cursor.load(Ordering::SeqCst));
        assert_eq!(listing.sessions.len(), 2);
        assert_eq!(listing.sessions[0].session_id, "first-page");
        assert_eq!(listing.sessions[1].session_id, "second-page");

        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_listing_reports_delete_capability() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_list_with_delete_capability(agent_side));

        let listing = list_sessions_via_transport(client_transport, PathBuf::from("/tmp"))
            .await
            .expect("session listing should include delete capability");

        assert!(listing.delete_supported);
        assert_eq!(listing.sessions.len(), 1);
        assert_eq!(listing.sessions[0].session_id, "delete-capable-session");

        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_deletion_sends_delete_request() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let delete_seen = Arc::new(AtomicBool::new(false));
        let agent_task = tokio::spawn(run_mock_agent_delete_session(
            agent_side,
            delete_seen.clone(),
        ));

        delete_session_via_transport(client_transport, "delete-me".to_string())
            .await
            .expect("session deletion should succeed");

        assert!(delete_seen.load(Ordering::SeqCst));
        agent_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_deletion_authenticates_and_retries_delete() {
        let (client_side, agent_side) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_side);
        let client_transport = ByteStreams::new(cw.compat_write(), cr.compat());
        let agent_task = tokio::spawn(run_mock_agent_delete_auth_required_then_authenticates(
            agent_side,
        ));

        delete_session_via_transport(client_transport, "delete-me".to_string())
            .await
            .expect("session deletion should authenticate and retry");

        agent_task.abort();
    }
}
