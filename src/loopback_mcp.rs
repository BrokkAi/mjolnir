//! Shared in-process loopback MCP endpoint for council tool servers.
//!
//! The handlers must run inside mj (they stream UI events and share runtime
//! state), so they listen on a bearer-token-guarded loopback HTTP port — but
//! ACP adapters are pointed at a stdio `mj mcp-proxy` command that forwards
//! to that port, because stdio is the one MCP transport every agent must
//! support. Dropping the server cancels the listener and every open session.

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{EnvVariable, McpServer, McpServerStdio};
use anyhow::{Context, Result, anyhow};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use rmcp::{
    ServerHandler,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::mcp_proxy;

const MCP_PATH: &str = "/mcp";

pub struct LoopbackServer {
    advertised: McpServer,
    tools_listed: watch::Receiver<bool>,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl LoopbackServer {
    /// Serve `handler` on a fresh loopback port and return the stdio
    /// `mj mcp-proxy` advertisement for it. `label` names the endpoint in
    /// errors and logs; `tools_listed` should flip to true when the handler
    /// sees its first `tools/list`.
    pub async fn start<H>(
        server_name: &str,
        label: &'static str,
        handler: H,
        tools_listed: watch::Receiver<bool>,
    ) -> Result<Self>
    where
        H: ServerHandler + Clone,
    {
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes)
            .map_err(|error| anyhow!("generate {label} MCP bearer token: {error}"))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
        let authorization = format!("Bearer {token}");
        let cancellation = CancellationToken::new();
        let mut config = StreamableHttpServerConfig::default();
        config.cancellation_token = cancellation.clone();
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let protected = axum::Router::new().nest_service(MCP_PATH, service).layer(
            axum::middleware::from_fn_with_state(authorization, require_bearer),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .with_context(|| format!("bind {label} MCP listener"))?;
        let addr = listener
            .local_addr()
            .with_context(|| format!("read {label} MCP listener address"))?;
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, protected)
                .with_graceful_shutdown(task_cancellation.cancelled_owned())
                .await
            {
                tracing::warn!("{label} MCP listener stopped: {error}");
            }
        });
        let exe = std::env::current_exe()
            .with_context(|| format!("resolve the mj executable for the {label} MCP proxy"))?;
        let mut stdio = McpServerStdio::new(server_name, exe);
        stdio.args = vec![
            "mcp-proxy".to_string(),
            "--url".to_string(),
            format!("http://{addr}{MCP_PATH}"),
        ];
        stdio.env = vec![EnvVariable::new(mcp_proxy::TOKEN_ENV, token)];
        Ok(Self {
            advertised: McpServer::Stdio(stdio),
            tools_listed,
            cancellation,
            task,
        })
    }

    pub fn advertised(&self) -> &McpServer {
        &self.advertised
    }

    /// Resolve once the agent side has listed the handler's tools;
    /// `timeout_error`/`closed_error` describe the failing agent in the
    /// caller's vocabulary.
    pub async fn wait_until_tools_listed(
        &self,
        timeout: Duration,
        timeout_error: &'static str,
        closed_error: &'static str,
    ) -> Result<()> {
        let mut tools_listed = self.tools_listed.clone();
        if *tools_listed.borrow() {
            return Ok(());
        }
        tokio::time::timeout(timeout, tools_listed.changed())
            .await
            .map_err(|_| anyhow!(timeout_error))?
            .map_err(|_| anyhow!(closed_error))?;
        Ok(())
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

async fn require_bearer(
    State(expected): State<String>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, (StatusCode, &'static str)> {
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.as_bytes() == expected.as_bytes());
    if authorized {
        Ok(next.run(request).await)
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}
