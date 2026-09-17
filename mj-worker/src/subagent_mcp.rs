//! Mjolnir-owned delegation tools for Claude and Codex parent sessions.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use mj_core::subagent::{
    DEFAULT_WAIT_SECONDS, FileSourceRanges, MAX_WAIT_SECONDS, SubagentToolAction,
    SubagentToolRequest,
};

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

/// How long any action other than `wait` may take to be answered. The daemon
/// must notice the queued request, run the action (a spawn may provision a
/// child session) and complete it back to the worker.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// Slack added to a `wait` call's own timeout so the daemon's bounded wait
/// can finish and be relayed before the shim gives up.
const WAIT_REPLY_GRACE: Duration = Duration::from_secs(60);

/// How long the shim waits for the worker to answer `action`. A `wait` blocks
/// for as long as the caller asked plus grace; everything else is bounded by
/// [`REPLY_TIMEOUT`].
fn reply_timeout(action: &SubagentToolAction) -> Duration {
    match action {
        SubagentToolAction::WaitAgents {
            timeout_seconds, ..
        } => {
            Duration::from_secs(
                timeout_seconds
                    .unwrap_or(DEFAULT_WAIT_SECONDS)
                    .clamp(1, MAX_WAIT_SECONDS),
            ) + WAIT_REPLY_GRACE
        }
        _ => REPLY_TIMEOUT,
    }
}

/// The tool error when the worker never answered within the budget. The
/// request may still be queued, so the model is told how to collect it.
fn unanswered_reply(request_id: &str, waited: Duration) -> Value {
    json!({
        "request_id": request_id,
        "error": format!(
            "Mjolnir did not answer this request within {} seconds. It may still be queued; repeat this call with the same request_key to collect its result. If this was a spawn without a request_key, check list_agents before spawning again so the child is not duplicated.",
            waited.as_secs()
        )
    })
}

pub fn run_mcp_stdio(socket: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    run(stdin.lock(), std::io::stdout(), socket)
}

/// Serve MCP over `reader`/`writer` against the worker `socket`. Calls are
/// dispatched concurrently, each on its own socket connection, so a long
/// `wait` never blocks a cheap `list_agents` queued after it.
fn run<R: BufRead, W: Write + Send + 'static>(reader: R, writer: W, socket: &Path) -> Result<()> {
    let socket = socket.to_path_buf();
    crate::mcp_stdio::serve(
        reader,
        writer,
        crate::mcp_stdio::McpServer {
            name: "mj-agents",
            instructions: SERVER_INSTRUCTIONS,
            tools: tool_definitions(),
            dispatch: crate::mcp_stdio::Dispatch::Concurrent,
            call: move |params: Option<&Value>| call(&socket, params),
        },
    )
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
    files: Vec<FileSourceRanges>,
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
    call_with_budget(socket, params, reply_timeout)
}

/// Answer one tool call, waiting for the worker as long as `budget` allows
/// for the call's action.
fn call_with_budget(
    socket: &Path,
    params: Option<&Value>,
    budget: impl Fn(&SubagentToolAction) -> Duration,
) -> Result<(Value, bool)> {
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
    let timeout = budget(&action);
    let request = SubagentToolRequest {
        request_id: request_id.clone(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action,
    };
    let Some(reply) = send(socket, &request, timeout)? else {
        return Ok((unanswered_reply(&request_id, timeout), true));
    };
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

/// `None` means the worker did not answer within `timeout`.
fn send(socket: &Path, request: &SubagentToolRequest, timeout: Duration) -> Result<Option<Value>> {
    crate::mcp_stdio::socket_request(socket, request, "sub-agent", timeout)
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
            "Start an independent Mjolnir child session in this session's target and filesystem. Returns child_session_id at once; the child runs on its own. Do other work, then collect its result with wait. If a model or effort you name cannot be checked when you spawn, an unsupported value is reported later as the child's startup error through wait or list_agents.",
            json!({
                "type":"object",
                "properties":{
                    "task_name":{"type":"string"},"instructions":{"type":"string"},
                    "profile_id":{"type":"string"},"model":{"type":"string"},"effort":{"type":"string"},
                    "working_directory":{"type":"string","description":"Launch directory for the child session on the parent's target. Absolute paths are used as-is; relative paths resolve against the parent session's working directory. The directory must exist; no other restriction applies. Defaults to the parent session's working directory."},"context":{"type":"string"},"request_key":{"type":"string","description":"Optional idempotency key. Repeating a call with the same key returns the original result instead of duplicating the work; useful for retries and long waits."},
                    "files":{"type":"array","description":"Source excerpts to include in the child's first prompt, grouped by file. Each entry names one relative file and a list of one or more one-based, inclusive line ranges to pull from it.","items":{"type":"object","properties":{"file":{"type":"string"},"ranges":{"type":"array","minItems":1,"items":{"type":"object","properties":{"start":{"type":"integer","minimum":1},"end":{"type":"integer","minimum":1}},"required":["start","end"],"additionalProperties":false}}},"required":["file","ranges"],"additionalProperties":false}}
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};

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
    fn wait_calls_get_their_own_timeout_plus_grace() {
        let wait = |timeout_seconds| SubagentToolAction::WaitAgents {
            child_session_ids: vec!["c1".into()],
            timeout_seconds,
        };
        assert_eq!(
            reply_timeout(&wait(Some(7))),
            Duration::from_secs(7) + WAIT_REPLY_GRACE
        );
        assert_eq!(
            reply_timeout(&wait(None)),
            Duration::from_secs(DEFAULT_WAIT_SECONDS) + WAIT_REPLY_GRACE
        );
        assert_eq!(
            reply_timeout(&wait(Some(MAX_WAIT_SECONDS * 2))),
            Duration::from_secs(MAX_WAIT_SECONDS) + WAIT_REPLY_GRACE
        );
        assert_eq!(
            reply_timeout(&SubagentToolAction::ListAgents),
            REPLY_TIMEOUT
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unanswered_call_becomes_a_tool_error_instead_of_hanging() {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        // Fake worker: consume the request, then hold the connection open
        // without ever answering.
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            std::thread::sleep(Duration::from_secs(2));
            let _ = reader.into_inner().read(&mut [0u8; 1]);
        });

        let started = std::time::Instant::now();
        let (value, is_error) =
            call_with_budget(&socket, Some(&json!({"name": "list_agents"})), |_| {
                Duration::from_millis(200)
            })
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "the call must give up at its budget, took {:?}",
            started.elapsed()
        );
        assert!(is_error, "{value}");
        assert!(
            !value["request_id"].as_str().unwrap_or_default().is_empty(),
            "{value}"
        );
        let error = value["error"].as_str().expect("error text");
        assert!(
            error.contains("did not answer") && error.contains("request_key"),
            "{error}"
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
