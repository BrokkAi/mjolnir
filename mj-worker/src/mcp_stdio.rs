//! The JSON-lines stdio transport shared by the worker's MCP servers.
//!
//! Hel's MCP servers (project memory, review dispatch, sub-agents) are
//! hand-rolled rather than built on an SDK. They differ only in their name,
//! instructions, tools and call handler, so the JSON-RPC loop lives here once.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// How often a long call reports that it is still working, when the client
/// asked to be kept informed. Claude Code abandons a stdio MCP call that sends
/// neither a response nor a progress notification for 1800 seconds, so a call
/// that reports every half minute never runs into that limit.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

pub struct McpServer<F> {
    pub name: &'static str,
    pub instructions: &'static str,
    pub tools: Vec<Value>,
    pub dispatch: Dispatch,
    /// How often an in-flight call may report progress. Tests shorten it; in
    /// production it is [`PROGRESS_INTERVAL`].
    pub progress_interval: Duration,
    /// Answer one `tools/call`: the structured result and whether it is a tool
    /// error the model can correct. `Err` becomes a JSON-RPC invalid-params
    /// error. The [`Progress`] handle reports that a slow call is still
    /// working; a handler with nothing useful to say ignores it.
    pub call: F,
}

/// Reports that one in-flight `tools/call` is still working.
///
/// MCP only allows progress for a request whose caller asked for it, by
/// putting a `progressToken` in the call's `_meta`. Without that token this
/// handle does nothing, so a client that never asked is never sent anything it
/// did not expect.
pub struct Progress {
    token: Option<Value>,
    interval: Duration,
    emit: Box<dyn Fn(&Value) + Send + Sync>,
}

impl Progress {
    /// How often [`Self::notify`] should be called while a call is in flight.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// A handle that reports nothing, for tests that call a handler directly
    /// with no client behind it.
    #[cfg(test)]
    pub fn silent(interval: Duration) -> Self {
        Self {
            token: None,
            interval,
            emit: Box::new(|_| {}),
        }
    }

    /// Send one `notifications/progress`. `elapsed` is how long the call has
    /// been running and `total` is the deadline it is working towards, both in
    /// seconds, so a client can show how far along the call is.
    pub fn notify(&self, elapsed: u64, total: Option<u64>, message: &str) {
        let Some(token) = &self.token else {
            return;
        };
        let mut params = json!({
            "progressToken": token,
            "progress": elapsed,
            "message": message,
        });
        if let Some(total) = total
            && let Some(object) = params.as_object_mut()
        {
            object.insert("total".into(), json!(total));
        }
        (self.emit)(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": params,
        }));
    }
}

/// Serve `server` over `reader` and `writer` until the reader closes, then let
/// in-flight concurrent calls finish.
pub fn serve<R, W, F>(reader: R, writer: W, server: McpServer<F>) -> Result<()>
where
    R: BufRead,
    W: Write + Send + Sync + 'static,
    F: Fn(Option<&Value>, &Progress) -> Result<(Value, bool)> + Send + Sync + 'static,
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
                let token = params
                    .as_ref()
                    .and_then(|params| params.pointer("/_meta/progressToken"))
                    .cloned();
                match server.dispatch {
                    Dispatch::Sequential => {
                        let progress = progress_handle(&output, token, server.progress_interval);
                        tool_response(id, call(params.as_ref(), &progress))
                    }
                    Dispatch::Concurrent => {
                        let output = Arc::clone(&output);
                        let call = Arc::clone(&call);
                        let interval = server.progress_interval;
                        calls.push(std::thread::spawn(move || {
                            let progress = progress_handle(&output, token, interval);
                            let response = tool_response(id, call(params.as_ref(), &progress));
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

/// Bind a progress handle to this server's output stream. Every notification
/// it writes goes through the same lock as a response, so lines never
/// interleave.
fn progress_handle<W: Write + Send + Sync + 'static>(
    output: &Arc<Mutex<W>>,
    token: Option<Value>,
    interval: Duration,
) -> Progress {
    let output = Arc::clone(output);
    Progress {
        token,
        interval,
        emit: Box::new(move |value| {
            if let Err(error) = write_line(&output, value) {
                tracing::warn!(%error, "could not write an MCP progress notification");
            }
        }),
    }
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

/// How long a connect may wait for the worker to accept. A Unix socket
/// connect only blocks when the listener's backlog is full, which means the
/// worker has stopped accepting.
#[cfg(unix)]
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Send one JSON request line over a worker Unix socket and read one JSON
/// reply line. `what` names the exchange in errors. The connect is bounded
/// by [`CONNECT_TIMEOUT`]; the reply must arrive within `reply_timeout`, and
/// `Ok(None)` reports that it did not, so a worker that never answers turns
/// into a bounded failure instead of an indefinite hang.
#[cfg(unix)]
pub fn socket_request<Q, A>(
    socket: &Path,
    request: &Q,
    what: &str,
    reply_timeout: Duration,
) -> Result<Option<A>>
where
    Q: serde::Serialize,
    A: serde::de::DeserializeOwned,
{
    let mut stream = connect_with_timeout(socket)
        .with_context(|| format!("connect to the {what} socket {}", socket.display()))?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    stream
        .write_all(&body)
        .with_context(|| format!("send the {what} request"))?;
    stream
        .flush()
        .with_context(|| format!("flush the {what} request"))?;
    stream
        .set_read_timeout(Some(reply_timeout))
        .with_context(|| format!("bound the {what} reply wait"))?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => anyhow::bail!("the {what} socket closed without a reply"),
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error).with_context(|| format!("read the {what} reply")),
    }
    serde_json::from_str(line.trim())
        .map(Some)
        .with_context(|| format!("parse the {what} reply"))
}

/// Connect on a helper thread so the wait is bounded. A thread still blocked
/// in connect after the timeout is abandoned; it exits once the connect
/// resolves.
#[cfg(unix)]
fn connect_with_timeout(socket: &Path) -> Result<std::os::unix::net::UnixStream> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let path = socket.to_path_buf();
    std::thread::spawn(move || {
        let _ = sender.send(mj_core::local_sockets::connect_unix_stream(&path));
    });
    match receiver.recv_timeout(CONNECT_TIMEOUT) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            anyhow::bail!("no accept within {}s", CONNECT_TIMEOUT.as_secs())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("the connect thread ended without a result")
        }
    }
}

/// Workers run on Unix; the servers compile everywhere so the CLI stays one
/// shape, and say plainly where they cannot run.
#[cfg(not(unix))]
pub fn socket_request<Q, A>(
    socket: &Path,
    _request: &Q,
    what: &str,
    _reply_timeout: Duration,
) -> Result<Option<A>> {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    /// Collects everything the server writes, so a test can read it after
    /// `serve` has consumed the writer.
    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("shared writer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Serve one request with a tool that sleeps for `work`, reporting
    /// progress on every tick, and return everything the server wrote.
    fn serve_a_slow_call(request: Value, interval: Duration, work: Duration) -> Vec<Value> {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let input = format!("{request}\n");
        serve(
            input.as_bytes(),
            SharedWriter(Arc::clone(&buffer)),
            McpServer {
                name: "test",
                instructions: "test",
                tools: vec![json!({"name": "slow", "inputSchema": {"type": "object"}})],
                dispatch: Dispatch::Concurrent,
                progress_interval: interval,
                call: move |_params: Option<&Value>, progress: &Progress| {
                    let started = Instant::now();
                    while started.elapsed() < work {
                        std::thread::sleep(progress.interval());
                        progress.notify(started.elapsed().as_secs(), Some(60), "still working");
                    }
                    Ok((json!({"done": true}), false))
                },
            },
        )
        .expect("the server serves the request");
        written_lines(&buffer)
    }

    fn written_lines(buffer: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
        let bytes = buffer.lock().expect("buffer poisoned").clone();
        String::from_utf8(bytes)
            .expect("utf8 output")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each line is one JSON message"))
            .collect()
    }

    #[test]
    fn a_call_with_a_progress_token_gets_progress_lines_before_its_response() {
        let lines = serve_a_slow_call(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                   "params":{"name":"slow","arguments":{},"_meta":{"progressToken":"tok-7"}}}),
            Duration::from_millis(20),
            Duration::from_millis(120),
        );
        let notifications = lines
            .iter()
            .filter(|line| line["method"] == "notifications/progress")
            .collect::<Vec<_>>();
        assert!(
            notifications.len() >= 2,
            "a slow call must report more than once: {lines:?}"
        );
        assert_eq!(notifications[0]["params"]["progressToken"], "tok-7");
        assert_eq!(notifications[0]["params"]["total"], 60);
        assert_eq!(notifications[0]["params"]["message"], "still working");
        assert!(
            lines.last().expect("a response")["result"]["structuredContent"]["done"] == true,
            "the response must come last: {lines:?}"
        );
    }

    #[test]
    fn a_call_without_a_progress_token_is_answered_with_no_notifications() {
        let lines = serve_a_slow_call(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                   "params":{"name":"slow","arguments":{}}}),
            Duration::from_millis(20),
            Duration::from_millis(80),
        );
        assert!(
            lines.iter().all(|line| line["method"].is_null()),
            "a client that asked for no progress must be sent none: {lines:?}"
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
    }

    /// Accept one connection, read the request line, then run `answer` on
    /// the stream and hold it open until the client goes away.
    fn fake_worker(
        socket: &Path,
        answer: impl FnOnce(&mut std::os::unix::net::UnixStream) + Send + 'static,
    ) {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut stream = reader.into_inner();
            answer(&mut stream);
            let _ = stream.read(&mut [0u8; 1]);
        });
    }

    #[test]
    fn socket_request_reports_a_missing_reply_within_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        fake_worker(&socket, |_| std::thread::sleep(Duration::from_secs(2)));

        let started = Instant::now();
        let reply: Option<Value> = socket_request(
            &socket,
            &json!({"ping": true}),
            "test",
            Duration::from_millis(200),
        )
        .unwrap();
        assert_eq!(reply, None);
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "the call must give up at the reply timeout, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn socket_request_returns_the_reply_when_it_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        fake_worker(&socket, |stream| {
            stream.write_all(b"{\"pong\":true}\n").unwrap();
            stream.flush().unwrap();
        });

        let reply: Option<Value> = socket_request(
            &socket,
            &json!({"ping": true}),
            "test",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(reply, Some(json!({"pong": true})));
    }

    #[test]
    fn socket_request_reports_a_closed_socket_as_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            // Drop without answering.
        });

        let error = socket_request::<_, Value>(
            &socket,
            &json!({"ping": true}),
            "test",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("closed without a reply"),
            "{error:#}"
        );
    }
}
