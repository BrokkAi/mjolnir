//! Stdio ↔ loopback streamable-HTTP MCP bridge.
//!
//! ACP adapters spawn `mj mcp-proxy --url <endpoint>` as an ordinary stdio
//! MCP server — the one transport every agent must support — while the
//! council's tool servers keep running inside the main mj process, where they
//! stream UI events and share runtime state. The proxy forwards tool listing
//! and calls one-to-one and mirrors the backend's server info, so an adapter
//! cannot tell it apart from a native stdio server. The bearer token for the
//! loopback endpoint arrives in [`TOKEN_ENV`], never on argv.

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleClient, RunningService, ServiceError},
    transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    },
};

/// Carries the loopback bearer token from mj to the spawned proxy. An env
/// variable rather than argv so the secret never shows up in process listings.
pub const TOKEN_ENV: &str = "MJ_MCP_PROXY_TOKEN";

/// Run until stdin closes or the backend session ends.
pub async fn serve(url: &str) -> Result<()> {
    let token = std::env::var(TOKEN_ENV)
        .with_context(|| format!("{TOKEN_ENV} is not set; mcp-proxy is only spawned by mj"))?;
    let backend = connect_backend(url, &token).await?;
    let server = Proxy::new(backend)
        .serve(rmcp::transport::stdio())
        .await
        .context("serve MCP over stdio")?;
    server.waiting().await.context("stdio MCP proxy stopped")?;
    Ok(())
}

async fn connect_backend(url: &str, token: &str) -> Result<RunningService<RoleClient, ()>> {
    let transport = StreamableHttpClientTransport::with_client(
        reqwest13::Client::default(),
        StreamableHttpClientTransportConfig::with_uri(url.to_owned()).auth_header(token),
    );
    ()
        .serve(transport)
        .await
        .context("connect to loopback MCP endpoint")
}

/// Mirrors the backend server one-to-one. Keeps the backend client alive for
/// as long as the stdio side is being served.
#[derive(Clone)]
struct Proxy {
    backend: Arc<RunningService<RoleClient, ()>>,
}

impl Proxy {
    fn new(backend: RunningService<RoleClient, ()>) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }
}

impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerInfo {
        self.backend
            .peer_info()
            .map(|info| (*info).clone())
            .unwrap_or_else(|| {
                ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            })
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        self.backend
            .list_tools(request)
            .await
            .map_err(forwarded_error)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        tokio::select! {
            result = self.backend.call_tool(request) => result.map_err(forwarded_error),
            _ = context.ct.cancelled() => Err(McpError::internal_error(
                "tool call cancelled by the MCP client",
                None,
            )),
        }
    }
}

fn forwarded_error(error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(data) => data,
        other => McpError::internal_error(other.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopback_mcp::LoopbackServer;
    use agent_client_protocol::schema::v1::McpServer;
    use rmcp::{
        handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
        model::{Content, Implementation},
        tool, tool_router,
    };
    use schemars::JsonSchema;
    use serde::Deserialize;
    use std::time::Duration;
    use tokio::sync::watch;

    #[derive(Debug, Deserialize, JsonSchema)]
    struct EchoArgs {
        text: String,
    }

    #[derive(Clone)]
    struct EchoHandler {
        tools_listed: watch::Sender<bool>,
        tool_router: ToolRouter<Self>,
    }

    #[tool_router(router = tool_router)]
    impl EchoHandler {
        fn new(tools_listed: watch::Sender<bool>) -> Self {
            Self {
                tools_listed,
                tool_router: Self::tool_router(),
            }
        }

        #[tool(name = "echo", description = "echo the text back")]
        async fn echo(
            &self,
            Parameters(args): Parameters<EchoArgs>,
        ) -> std::result::Result<CallToolResult, McpError> {
            Ok(CallToolResult::success(vec![Content::text(format!(
                "echo: {}",
                args.text
            ))]))
        }
    }

    impl ServerHandler for EchoHandler {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new("mj-test-echo", "0.0.0"))
                .with_instructions("ECHO CONTRACT")
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> std::result::Result<ListToolsResult, McpError> {
            let _ = self.tools_listed.send(true);
            Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> std::result::Result<CallToolResult, McpError> {
            self.tool_router
                .call(ToolCallContext::new(self, request, context))
                .await
        }
    }

    async fn start_echo_server() -> (LoopbackServer, String, String) {
        let (tools_listed_tx, tools_listed) = watch::channel(false);
        let server = LoopbackServer::start(
            "mj-test-echo",
            "test-echo",
            EchoHandler::new(tools_listed_tx),
            tools_listed,
        )
        .await
        .expect("start loopback server");
        let McpServer::Stdio(stdio) = server.advertised() else {
            panic!("loopback server must advertise a stdio proxy command");
        };
        assert_eq!(stdio.args[0], "mcp-proxy");
        assert_eq!(stdio.args[1], "--url");
        let url = stdio.args[2].clone();
        let token = stdio
            .env
            .iter()
            .find(|var| var.name == TOKEN_ENV)
            .map(|var| var.value.clone())
            .expect("advertisement must carry the proxy token env var");
        (server, url, token)
    }

    #[tokio::test]
    async fn stdio_proxy_round_trips_tools_and_mirrors_backend_info() {
        let (server, url, token) = start_echo_server().await;

        let backend = connect_backend(&url, &token)
            .await
            .expect("connect to loopback backend");
        let (client_io, proxy_io) = tokio::io::duplex(64 * 1024);
        let (proxy_read, proxy_write) = tokio::io::split(proxy_io);
        // serve() awaits the client's initialize handshake, so it must run
        // concurrently with the client's own serve() below.
        let stdio_server =
            tokio::spawn(Proxy::new(backend).serve((proxy_read, proxy_write)));
        let (client_read, client_write) = tokio::io::split(client_io);
        let client = ()
            .serve((client_read, client_write))
            .await
            .expect("connect stdio client to proxy");
        let stdio_server = stdio_server
            .await
            .expect("proxy serve task")
            .expect("serve proxy over in-memory stdio");

        let info = client.peer_info().expect("proxy advertises server info");
        assert_eq!(info.instructions.as_deref(), Some("ECHO CONTRACT"));

        let tools = client.list_tools(None).await.expect("list tools");
        assert_eq!(tools.tools.len(), 1);
        assert_eq!(tools.tools[0].name, "echo");
        server
            .wait_until_tools_listed(Duration::from_secs(5), "timed out", "closed")
            .await
            .expect("backend handler observed tools/list");

        let result = client
            .call_tool(CallToolRequestParams::new("echo").with_arguments(
                serde_json::json!({"text": "hi"}).as_object().cloned().unwrap(),
            ))
            .await
            .expect("call echo through the proxy");
        let text = result
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|text| text.text.clone())
            .unwrap_or_default();
        assert_eq!(text, "echo: hi");

        drop(client);
        drop(stdio_server);
    }

    #[tokio::test]
    async fn loopback_endpoint_rejects_a_wrong_bearer_token() {
        let (_server, url, _token) = start_echo_server().await;
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            connect_backend(&url, "wrong-token"),
        )
        .await
        .expect("connect attempt must not hang");
        assert!(error.is_err(), "wrong bearer token must be rejected");
    }
}
