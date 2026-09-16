//! Mjolnir-owned delegation tools for Claude and Codex parent sessions.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use mj_core::subagent::{MAX_WAIT_SECONDS, SourceRange, SubagentToolAction, SubagentToolRequest};

/// Server instructions stating the spawn/wait contract: results reach the
/// model only as the `wait` tool call's own answer, never as a push.
const SERVER_INSTRUCTIONS: &str = "Delegate work to Mjolnir child sessions in this target. spawn starts a child and returns its child_session_id immediately; the child runs independently while you continue other work. Collect a child's result only by calling wait, which blocks until the named children finish their current turn or the timeout. The user can see every child in the Sub-agents workspace.";

/// The degraded answer when the daemon has not completed the request within
/// the socket ceiling. The request stays queued; the model must collect the
/// result itself, because nothing is ever pushed into its conversation.
fn pending_reply(request_id: &str) -> Value {
    json!({
        "request_id":request_id,
        "accepted":true,
        "note":"Mjolnir has not answered this request yet; it stays queued. Repeat this call with the same request_key to collect its result. If this was a spawn without a request_key, check list_agents before spawning again so the child is not duplicated."
    })
}

pub fn run_mcp_stdio(socket: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    run(stdin.lock(), std::io::stdout(), socket)
}

/// Serve MCP over `reader`/`writer` against the worker `socket`.
///
/// Each `tools/call` runs on its own thread and its own socket connection, so a
/// long `wait` never head-of-line blocks a cheap `list_agents` queued after it.
/// JSON-RPC lets a response arrive in any order because it carries the request
/// id, and every write to `writer` is serialized behind one lock so concurrent
/// responses never interleave.
fn run<R: BufRead, W: Write + Send + 'static>(reader: R, writer: W, socket: &Path) -> Result<()> {
    let output = Arc::new(Mutex::new(writer));
    let socket = socket.to_path_buf();
    let mut calls = Vec::new();
    for line in reader.lines() {
        let line = line.context("read sub-agent MCP request")?;
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
        match method {
            "initialize" => write_line(
                &output,
                &rpc_result(
                    id,
                    json!({
                        "protocolVersion": request.pointer("/params/protocolVersion").cloned().unwrap_or_else(|| json!("2025-03-26")),
                        "capabilities":{"tools":{"listChanged":false}},
                        "serverInfo":{"name":"mj-agents","version":env!("CARGO_PKG_VERSION")},
                        "instructions": SERVER_INSTRUCTIONS
                    }),
                ),
            )?,
            "ping" => write_line(&output, &rpc_result(id, json!({})))?,
            "tools/list" => write_line(
                &output,
                &rpc_result(id, json!({"tools": tool_definitions()})),
            )?,
            "tools/call" => {
                // Dispatch on its own thread and socket connection. The read
                // loop stays free to accept and dispatch the next request while
                // this call blocks on the worker (a `wait` can block for up to
                // an hour).
                let output = Arc::clone(&output);
                let socket = socket.clone();
                let params = request.get("params").cloned();
                calls.push(std::thread::spawn(move || {
                    let response = match call(&socket, params.as_ref()) {
                        Ok((value, is_error)) => rpc_result(
                            id,
                            json!({
                                "content":[{"type":"text","text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())}],
                                "structuredContent":value,
                                "isError":is_error
                            }),
                        ),
                        Err(error) => rpc_error(id, -32602, format!("{error:#}")),
                    };
                    let _ = write_line(&output, &response);
                }));
            }
            _ => write_line(
                &output,
                &rpc_error(id, -32601, format!("unknown MCP method {method:?}")),
            )?,
        }
    }
    // stdin closed: the harness is gone. Let in-flight calls finish so their
    // worker sockets close cleanly rather than being torn down mid-exchange.
    for call in calls {
        let _ = call.join();
    }
    Ok(())
}

#[derive(Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Deserialize)]
struct SpawnArgs {
    task_name: String,
    instructions: String,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    working_directory: PathBuf,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    files: Vec<SourceRange>,
    #[serde(default)]
    request_key: Option<String>,
}

#[derive(Deserialize)]
struct ChildArgs {
    child_session_id: String,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct WaitArgs {
    child_session_ids: Vec<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

fn call(socket: &Path, params: Option<&Value>) -> Result<(Value, bool)> {
    let params: CallParams = serde_json::from_value(params.cloned().context("missing params")?)?;
    let (action, supplied_key) = match params.name.as_str() {
        "list_profiles" => (SubagentToolAction::ListProfiles, None),
        "spawn" => {
            let args: SpawnArgs = serde_json::from_value(params.arguments)?;
            let key = args.request_key.clone();
            (
                SubagentToolAction::Spawn {
                    task_name: args.task_name,
                    instructions: args.instructions,
                    profile_id: args.profile_id,
                    model: args.model,
                    effort: args.effort,
                    working_directory: args.working_directory,
                    context: args.context,
                    files: args.files,
                },
                key,
            )
        }
        "list_agents" => (SubagentToolAction::ListAgents, None),
        "send_input" => {
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            let message = args.message.context("send_input requires message")?;
            (
                SubagentToolAction::SendInput {
                    child_session_id: args.child_session_id,
                    message,
                },
                None,
            )
        }
        "wait" => {
            let args: WaitArgs = serde_json::from_value(params.arguments)?;
            (
                SubagentToolAction::WaitAgents {
                    child_session_ids: args.child_session_ids,
                    timeout_seconds: args.timeout_seconds,
                },
                None,
            )
        }
        "interrupt" | "close" => {
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            let action = if params.name == "interrupt" {
                SubagentToolAction::InterruptAgent {
                    child_session_id: args.child_session_id,
                }
            } else {
                SubagentToolAction::CloseAgent {
                    child_session_id: args.child_session_id,
                }
            };
            (action, None)
        }
        other => bail!("unknown sub-agent tool {other:?}"),
    };
    let request_id = supplied_key.unwrap_or(mj_core::state::new_session_id()?);
    let request = SubagentToolRequest {
        request_id: request_id.clone(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action,
    };
    let reply = send(socket, &request)?;
    if let Some(result) = reply.get("result").filter(|value| !value.is_null()) {
        return Ok((
            result.clone(),
            result
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ));
    }
    Ok((pending_reply(&request_id), false))
}

#[cfg(unix)]
fn send(socket: &Path, request: &SubagentToolRequest) -> Result<Value> {
    let mut stream = mj_core::local_sockets::connect_unix_stream(socket)
        .with_context(|| format!("connect to sub-agent socket {}", socket.display()))?;
    let mut body = serde_json::to_vec(request)?;
    body.push(b'\n');
    stream.write_all(&body).context("send sub-agent request")?;
    stream.flush().context("flush sub-agent request")?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("read sub-agent reply")?;
    serde_json::from_str(line.trim()).context("parse sub-agent reply")
}

#[cfg(not(unix))]
fn send(socket: &Path, _request: &SubagentToolRequest) -> Result<Value> {
    bail!(
        "sub-agent sockets are unavailable on this platform: {}",
        socket.display()
    )
}

fn tool_definitions() -> Vec<Value> {
    let child = json!({"type":"object","properties":{"child_session_id":{"type":"string"}},"required":["child_session_id"],"additionalProperties":false});
    vec![
        tool(
            "list_profiles",
            "List eligible sub-agent profiles and their available model selectors.",
            json!({"type":"object","additionalProperties":false}),
        ),
        tool(
            "spawn",
            "Start an independent Mjolnir child session in this session's target and filesystem. Returns child_session_id at once; the child runs on its own. Do other work, then collect its result with wait.",
            json!({
                "type":"object",
                "properties":{
                    "task_name":{"type":"string"},"instructions":{"type":"string"},
                    "profile_id":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},
                    "working_directory":{"type":"string","description":"Launch directory for the child session on the parent's target. Absolute paths are used as-is; relative paths resolve against the parent session's working directory. The directory must exist; no other restriction applies. Defaults to the parent session's working directory."},"context":{"type":"string"},"request_key":{"type":"string","description":"Optional idempotency key. Repeating a call with the same key returns the original result instead of duplicating the work; useful for retries and long waits."},
                    "files":{"type":"array","items":{"type":"object","properties":{"file":{"type":"string"},"start":{"type":"integer","minimum":1},"end":{"type":"integer","minimum":1}},"required":["file","start","end"],"additionalProperties":false}}
                },
                "required":["task_name","instructions"],"additionalProperties":false
            }),
        ),
        tool(
            "list_agents",
            "List this parent's Mjolnir child sessions and status.",
            json!({"type":"object","additionalProperties":false}),
        ),
        tool(
            "send_input",
            "Send follow-up input to one child session.",
            json!({"type":"object","properties":{"child_session_id":{"type":"string"},"message":{"type":"string"}},"required":["child_session_id","message"],"additionalProperties":false}),
        ),
        tool(
            "wait",
            "Block until the named child sessions finish their current turn, or until the timeout, then return each child's latest output. Spawn agents, do other work, then wait to collect results. timeout_seconds defaults to 300 and is capped at 3600.",
            json!({"type":"object","properties":{"child_session_ids":{"type":"array","items":{"type":"string"},"minItems":1},"timeout_seconds":{"type":"integer","minimum":1,"maximum":MAX_WAIT_SECONDS}},"required":["child_session_ids"],"additionalProperties":false}),
        ),
        tool(
            "interrupt",
            "Interrupt the current turn of one child.",
            child.clone(),
        ),
        tool(
            "close",
            "Stop one child session and retain its conversation.",
            child,
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input_schema})
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn write_line<W: Write>(output: &Mutex<W>, value: &Value) -> Result<()> {
    // Serialize into one buffer, then take the lock for a single write, so a
    // response from one call thread never interleaves with another's.
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    let mut output = output.lock().expect("sub-agent MCP stdout lock poisoned");
    output.write_all(&buf)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_advertises_the_shared_runtime_timeout_limit() {
        let wait = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "wait")
            .expect("wait definition");
        assert_eq!(
            wait["inputSchema"]["properties"]["timeout_seconds"]["maximum"],
            MAX_WAIT_SECONDS
        );
    }

    #[test]
    fn instructions_collect_results_through_wait_and_never_promise_a_push() {
        assert!(SERVER_INSTRUCTIONS.contains("wait"));
        assert!(
            !SERVER_INSTRUCTIONS.contains("arrive in"),
            "instructions must not promise pushed results: {SERVER_INSTRUCTIONS}"
        );
    }

    #[test]
    fn the_pending_reply_directs_retries_through_idempotency() {
        let reply = pending_reply("request-1");
        assert_eq!(reply["request_id"], "request-1");
        assert_eq!(reply["accepted"], true);
        let note = reply["note"].as_str().expect("note text");
        assert!(
            note.contains("request_key") && note.contains("list_agents"),
            "the note must route retries safely: {note}"
        );
        assert!(
            !note.contains("arrive in"),
            "the note must not promise pushed results: {note}"
        );
    }

    #[test]
    fn spawn_documents_its_idempotency_key() {
        let spawn = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "spawn")
            .expect("spawn definition");
        let description = spawn["inputSchema"]["properties"]["request_key"]["description"]
            .as_str()
            .expect("request_key description");
        assert!(description.contains("idempotency key"));
    }

    #[test]
    fn spawn_documents_how_a_working_directory_resolves() {
        let spawn = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "spawn")
            .expect("spawn definition");
        let description = spawn["inputSchema"]["properties"]["working_directory"]["description"]
            .as_str()
            .expect("working_directory description");
        assert!(description.contains("Absolute"), "{description}");
        assert!(description.contains("relative"), "{description}");
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_tool_call_does_not_block_a_later_one() {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;

        // Observe output after `run` consumes the writer.
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

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");

        // Fake worker: reply to any non-wait call at once, delay `wait_agents`,
        // and serve each accepted connection on its own thread so the delay
        // cannot serialize the two calls at the socket layer.
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let request: Value = serde_json::from_str(line.trim()).unwrap();
                    let action = request["action"]["action"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    if action == "wait_agents" {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    let reply = json!({"accepted": true, "result": {"action": action}});
                    let mut body = serde_json::to_vec(&reply).unwrap();
                    body.push(b'\n');
                    let mut stream = reader.into_inner();
                    stream.write_all(&body).unwrap();
                    stream.flush().unwrap();
                    // Hold the connection open until the client has read the
                    // reply, mirroring the real worker's `serve_one`.
                    let _ = stream.read(&mut [0u8; 1]);
                });
            }
        });

        // The slow `wait` is sent first, the cheap `list_agents` second. Serial
        // dispatch would answer `wait` first; concurrent dispatch answers the
        // cheap call first because it does not wait behind the slow one.
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"wait","arguments":{"child_session_ids":["c1"],"timeout_seconds":1}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_agents"}}),
        );
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        run(input.as_bytes(), SharedWriter(Arc::clone(&buffer)), &socket).unwrap();

        let written = buffer.lock().unwrap();
        let first: Value = serde_json::from_str(
            std::str::from_utf8(&written)
                .unwrap()
                .lines()
                .next()
                .expect("at least one response"),
        )
        .unwrap();
        assert_eq!(
            first["id"],
            2,
            "the cheap list_agents response must be written before the slow wait: {}",
            String::from_utf8_lossy(&written)
        );
    }
}
