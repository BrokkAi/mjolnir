//! Stdio transport for the in-process MCP tool servers.
//!
//! The subagent, memory, and review tool handlers need live access to this
//! process's state, so they cannot run as standalone child processes. Instead
//! the parent listens on an ephemeral loopback socket and advertises a stdio
//! MCP server whose command is this same `mj` binary running the hidden
//! `mcp-bridge` subcommand. The bridge authenticates with a single-line token
//! (passed via environment variable, never argv) and then pipes its
//! stdin/stdout to the socket verbatim: newline-delimited JSON-RPC flows
//! unchanged end to end, so the bridge never parses MCP.
//!
//! Every accepted connection is one MCP session, matching stdio semantics:
//! agents that respawn the server command get a fresh session against the
//! same shared handler state.

use std::time::Duration;

use agent_client_protocol::schema::v1::{EnvVariable, McpServer, McpServerStdio};
use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use rmcp::{ServerHandler, serve_server};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Environment variable carrying the connection token to the bridge child.
/// Passed through the advertised server's `env`, not argv, so it never shows
/// up in process listings.
pub const TOKEN_ENV: &str = "MJ_MCP_BRIDGE_TOKEN";

/// A connection that has not presented its token within this window is
/// dropped so stray connects cannot pin the listener.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The token line is 43 bytes of base64url plus a newline; anything longer is
/// garbage and gets the connection dropped before it can grow a buffer.
const MAX_TOKEN_LINE_BYTES: u64 = 256;

/// In-process MCP endpoint advertised to an ACP agent as a stdio server.
/// Dropping it closes the listener and every open MCP session.
pub struct BridgeServer {
    advertised: McpServer,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl BridgeServer {
    /// Serve `handler` to any bridge child that presents the token. The
    /// advertised command is the currently running executable, so the agent
    /// spawns `mj mcp-bridge --addr <loopback addr>` per MCP session.
    pub async fn start<H>(server_name: &str, handler: H) -> Result<Self>
    where
        H: ServerHandler + Clone,
    {
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes)
            .map_err(|error| anyhow::anyhow!("generate MCP bridge token: {error}"))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .with_context(|| format!("bind {server_name} MCP bridge listener"))?;
        let addr = listener
            .local_addr()
            .with_context(|| format!("read {server_name} MCP bridge listener address"))?;

        let cancellation = CancellationToken::new();
        let accept_cancellation = cancellation.clone();
        let expected = token.clone();
        let name_for_logs = server_name.to_string();
        let task = tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = accept_cancellation.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            tracing::warn!("{name_for_logs} MCP bridge listener stopped: {error}");
                            break;
                        }
                    },
                };
                let handler = handler.clone();
                let expected = expected.clone();
                let connection_cancellation = accept_cancellation.clone();
                let name = name_for_logs.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        serve_connection(stream, handler, &expected, connection_cancellation).await
                    {
                        tracing::debug!("{name} MCP bridge session ended: {error:#}");
                    }
                });
            }
        });

        let command =
            std::env::current_exe().context("resolve current executable for MCP bridge")?;
        let advertised = McpServer::Stdio(
            McpServerStdio::new(server_name, command)
                .args(vec![
                    "mcp-bridge".to_string(),
                    "--addr".to_string(),
                    addr.to_string(),
                ])
                .env(vec![EnvVariable::new(TOKEN_ENV, token)]),
        );
        Ok(Self {
            advertised,
            cancellation,
            task,
        })
    }

    pub fn advertised(&self) -> &McpServer {
        &self.advertised
    }

    pub async fn shutdown(mut self) {
        self.cancellation.cancel();
        let _ = (&mut self.task).await;
    }
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

async fn serve_connection<H>(
    stream: TcpStream,
    handler: H,
    expected_token: &str,
    cancellation: CancellationToken,
) -> Result<()>
where
    H: ServerHandler + Clone,
{
    stream.set_nodelay(true).ok();
    let (read, write) = stream.into_split();
    // Cap and time-limit the token read: nothing served until it matches.
    let mut limited = BufReader::new(read).take(MAX_TOKEN_LINE_BYTES);
    let mut line = String::new();
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, limited.read_line(&mut line)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(error).context("read MCP bridge token"),
        Err(_) => bail!("MCP bridge token handshake timed out"),
    }
    if line.trim_end_matches(['\r', '\n']) != expected_token {
        bail!("MCP bridge connection rejected: token mismatch");
    }
    // Hand the reader back with whatever the client pipelined after the token
    // line still buffered.
    let reader = limited.into_inner();
    let service = serve_server(handler, (reader, write))
        .await
        .context("serve MCP session over bridge socket")?;
    let session_cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    tokio::select! {
        _ = cancellation.cancelled() => {
            session_cancellation.cancel();
        }
        outcome = &mut waiting => {
            outcome.context("MCP bridge session task failed")?;
        }
    }
    Ok(())
}

/// Child half, run by the hidden `mj mcp-bridge` subcommand: authenticate to
/// the parent's loopback listener, then pipe stdin/stdout to the socket until
/// either side closes.
pub async fn run_bridge(addr: &str) -> Result<()> {
    let token = std::env::var(TOKEN_ENV)
        .with_context(|| format!("{TOKEN_ENV} must be set by the parent mj process"))?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    run_bridge_io(addr, &token, &mut stdin, &mut stdout).await
}

async fn run_bridge_io<R, W>(addr: &str, token: &str, input: &mut R, output: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect to mj MCP bridge listener at {addr}"))?;
    stream.set_nodelay(true).ok();
    let (mut socket_read, mut socket_write) = stream.into_split();
    socket_write
        .write_all(format!("{token}\n").as_bytes())
        .await
        .context("send MCP bridge token")?;

    let stdin_to_socket = async {
        let _ = tokio::io::copy(input, &mut socket_write).await;
        let _ = socket_write.shutdown().await;
    };
    let socket_to_stdout = async {
        let _ = tokio::io::copy(&mut socket_read, output).await;
        let _ = output.flush().await;
    };
    tokio::pin!(stdin_to_socket, socket_to_stdout);
    tokio::select! {
        // Parent closed the session: nothing more can arrive; exit now even
        // if the agent keeps our stdin open.
        _ = &mut socket_to_stdout => {}
        // Agent closed stdin: half-close the socket, then drain the parent's
        // remaining output until it closes its side.
        _ = &mut stdin_to_socket => {
            socket_to_stdout.await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::McpServer;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    };
    use rmcp::service::{RequestContext, RoleServer};
    use tokio::io::duplex;

    #[derive(Clone)]
    struct EchoHandler;

    impl ServerHandler for EchoHandler {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new("bridge-test", "0.0.0"))
        }

        fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = std::result::Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
        {
            std::future::ready(Ok(ListToolsResult::with_all_items(Vec::<Tool>::new())))
        }

        fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = std::result::Result<CallToolResult, rmcp::ErrorData>> + Send + '_
        {
            std::future::ready(Ok(CallToolResult::success(vec![])))
        }
    }

    fn stdio_parts(server: &BridgeServer) -> (String, String) {
        let McpServer::Stdio(stdio) = server.advertised() else {
            panic!("bridge must advertise a stdio MCP server");
        };
        let addr = stdio
            .args
            .iter()
            .skip_while(|arg| arg.as_str() != "--addr")
            .nth(1)
            .expect("advertised args carry --addr")
            .clone();
        let token = stdio
            .env
            .iter()
            .find(|var| var.name == TOKEN_ENV)
            .expect("advertised env carries the token")
            .value
            .clone();
        (addr, token)
    }

    #[tokio::test]
    async fn advertises_stdio_command_with_token_env() {
        let server = BridgeServer::start("bridge-test", EchoHandler)
            .await
            .expect("start bridge");
        let McpServer::Stdio(stdio) = server.advertised() else {
            panic!("bridge must advertise a stdio MCP server");
        };
        assert_eq!(stdio.name, "bridge-test");
        assert_eq!(stdio.args.first().map(String::as_str), Some("mcp-bridge"));
        let (addr, token) = stdio_parts(&server);
        assert!(addr.starts_with("127.0.0.1:"));
        assert!(!token.is_empty());
    }

    #[tokio::test]
    async fn wrong_token_is_rejected_before_serving() {
        let server = BridgeServer::start("bridge-test", EchoHandler)
            .await
            .expect("start bridge");
        let (addr, _token) = stdio_parts(&server);
        let mut stream = TcpStream::connect(&addr).await.expect("connect");
        stream
            .write_all(b"not-the-token\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
            .await
            .expect("write");
        let mut buf = Vec::new();
        let read = stream
            .read_to_end(&mut buf)
            .await
            .expect("read until close");
        assert_eq!(read, 0, "server must close without responding");
    }

    #[tokio::test]
    async fn serves_initialize_after_token_handshake() {
        let server = BridgeServer::start("bridge-test", EchoHandler)
            .await
            .expect("start bridge");
        let (addr, token) = stdio_parts(&server);
        let stream = TcpStream::connect(&addr).await.expect("connect");
        let (read, mut write) = stream.into_split();
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "fixture", "version": "1"}
            }
        });
        write
            .write_all(format!("{token}\n{initialize}\n").as_bytes())
            .await
            .expect("write handshake and initialize");
        let mut lines = BufReader::new(read).lines();
        let response = lines
            .next_line()
            .await
            .expect("read initialize response")
            .expect("connection stays open");
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("response is JSON");
        assert_eq!(response["id"], 1);
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            serde_json::json!("bridge-test")
        );
    }

    #[tokio::test]
    async fn bridge_io_forwards_input_then_drains_parent_output() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let parent = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("token"));
            assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("request"));
            write.write_all(b"response\n").await.expect("respond");
        });

        let mut input = b"request\n".as_slice();
        let mut output = Vec::new();
        run_bridge_io(&addr.to_string(), "token", &mut input, &mut output)
            .await
            .expect("bridge");
        parent.await.expect("parent");
        assert_eq!(output, b"response\n");
    }

    #[tokio::test]
    async fn bridge_io_exits_when_parent_closes_while_input_stays_open() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let parent = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (read, mut write) = stream.into_split();
            let token = BufReader::new(read).lines().next_line().await.unwrap();
            assert_eq!(token.as_deref(), Some("token"));
            write.write_all(b"done\n").await.expect("respond");
        });

        let (_input_writer, mut input) = duplex(64);
        let mut output = Vec::new();
        run_bridge_io(&addr.to_string(), "token", &mut input, &mut output)
            .await
            .expect("bridge");
        parent.await.expect("parent");
        assert_eq!(output, b"done\n");
    }

    #[tokio::test(start_paused = true)]
    async fn token_handshake_times_out_and_closes_the_connection() {
        let server = BridgeServer::start("bridge-test", EchoHandler)
            .await
            .expect("start bridge");
        let (addr, _token) = stdio_parts(&server);
        let mut stream = TcpStream::connect(&addr).await.expect("connect");

        tokio::time::advance(HANDSHAKE_TIMEOUT + Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.expect("read close");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn shutdown_closes_an_initialized_session_and_listener() {
        let server = BridgeServer::start("bridge-test", EchoHandler)
            .await
            .expect("start bridge");
        let (addr, token) = stdio_parts(&server);
        let stream = TcpStream::connect(&addr).await.expect("connect");
        let (read, mut write) = stream.into_split();
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "fixture", "version": "1"}
            }
        });
        write
            .write_all(format!("{token}\n{initialize}\n").as_bytes())
            .await
            .expect("initialize");
        let mut lines = BufReader::new(read).lines();
        lines
            .next_line()
            .await
            .expect("read initialize response")
            .expect("session remains open");

        server.shutdown().await;

        assert!(lines.next_line().await.expect("read close").is_none());
        assert!(TcpStream::connect(&addr).await.is_err());
    }
}
