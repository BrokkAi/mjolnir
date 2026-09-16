//! The JSON-lines stdio transport shared by the worker's MCP servers.
//!
//! Hel's MCP servers (project memory, review dispatch, sub-agents) are
//! hand-rolled rather than built on an SDK. They differ only in their name,
//! instructions, tools and call handler, so the JSON-RPC loop lives here once.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Whether `tools/call` requests are answered one at a time or concurrently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// Each call finishes before the next request is read.
    Sequential,
    /// Each call runs on its own thread, so a long call never blocks a cheap
    /// one queued after it. JSON-RPC responses carry their request id, so they
    /// may be written in any order.
    Concurrent,
}

pub struct McpServer<F> {
    pub name: &'static str,
    pub instructions: &'static str,
    pub tools: Vec<Value>,
    pub dispatch: Dispatch,
    /// Answer one `tools/call`: the structured result and whether it is a tool
    /// error the model can correct. `Err` becomes a JSON-RPC invalid-params
    /// error.
    pub call: F,
}

/// Serve `server` over `reader` and `writer` until the reader closes, then let
/// in-flight concurrent calls finish.
pub fn serve<R, W, F>(reader: R, writer: W, server: McpServer<F>) -> Result<()>
where
    R: BufRead,
    W: Write + Send + 'static,
    F: Fn(Option<&Value>) -> Result<(Value, bool)> + Send + Sync + 'static,
{
    let output = Arc::new(Mutex::new(writer));
    let call = Arc::new(server.call);
    let mut calls = Vec::new();
    for line in reader.lines() {
        let line = line.context("read MCP request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_line(&output, &rpc_error(Value::Null, -32700, error.to_string()))?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "initialize" => rpc_result(
                id,
                json!({
                    "protocolVersion": request.pointer("/params/protocolVersion").cloned().unwrap_or_else(|| json!("2025-03-26")),
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": server.name, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": server.instructions
                }),
            ),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({"tools": server.tools})),
            "tools/call" => {
                let params = request.get("params").cloned();
                match server.dispatch {
                    Dispatch::Sequential => tool_response(id, call(params.as_ref())),
                    Dispatch::Concurrent => {
                        let output = Arc::clone(&output);
                        let call = Arc::clone(&call);
                        calls.push(std::thread::spawn(move || {
                            let response = tool_response(id, call(params.as_ref()));
                            if let Err(error) = write_line(&output, &response) {
                                tracing::warn!(%error, "could not write an MCP tool response");
                            }
                        }));
                        continue;
                    }
                }
            }
            _ => rpc_error(id, -32601, format!("unknown MCP method {method:?}")),
        };
        write_line(&output, &response)?;
    }
    for call in calls {
        if call.join().is_err() {
            tracing::warn!("an MCP tool call thread panicked");
        }
    }
    Ok(())
}

fn tool_response(id: Value, result: Result<(Value, bool)>) -> Value {
    match result {
        Ok((structured, is_error)) => rpc_result(
            id,
            json!({
                "content": [{"type": "text", "text": serde_json::to_string_pretty(&structured).unwrap_or_else(|_| structured.to_string())}],
                "structuredContent": structured,
                "isError": is_error
            }),
        ),
        Err(error) => rpc_error(id, -32602, format!("{error:#}")),
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Send one JSON request line over a worker Unix socket and read one JSON
/// reply line. `what` names the exchange in errors.
#[cfg(unix)]
pub fn socket_request<Q, A>(socket: &std::path::Path, request: &Q, what: &str) -> Result<A>
where
    Q: serde::Serialize,
    A: serde::de::DeserializeOwned,
{
    let mut stream = mj_core::local_sockets::connect_unix_stream(socket)
        .with_context(|| format!("connect to the {what} socket {}", socket.display()))?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    stream
        .write_all(&body)
        .with_context(|| format!("send the {what} request"))?;
    stream
        .flush()
        .with_context(|| format!("flush the {what} request"))?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .with_context(|| format!("read the {what} reply"))?;
    serde_json::from_str(line.trim()).with_context(|| format!("parse the {what} reply"))
}

/// Workers run on Unix; the servers compile everywhere so the CLI stays one
/// shape, and say plainly where they cannot run.
#[cfg(not(unix))]
pub fn socket_request<Q, A>(socket: &std::path::Path, _request: &Q, what: &str) -> Result<A> {
    anyhow::bail!(
        "the {what} socket {} needs a Unix platform",
        socket.display()
    )
}

/// Serialize first and take the lock for one write, so concurrent responses
/// never interleave.
fn write_line<W: Write>(output: &Mutex<W>, value: &Value) -> Result<()> {
    let mut body = serde_json::to_vec(value)?;
    body.push(b'\n');
    let mut output = output.lock().expect("MCP stdout lock poisoned");
    output.write_all(&body).context("write MCP response")?;
    output.flush().context("flush MCP response")
}
