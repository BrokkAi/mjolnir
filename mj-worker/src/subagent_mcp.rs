//! Mjolnir-owned delegation tools for Claude and Codex parent sessions.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use mj_core::subagent::{MAX_WAIT_SECONDS, SourceRange, SubagentToolAction, SubagentToolRequest};

pub fn run_mcp_stdio(socket: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.context("read sub-agent MCP request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_line(
                    &mut output,
                    &rpc_error(Value::Null, -32700, error.to_string()),
                )?;
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
                    "capabilities":{"tools":{"listChanged":false}},
                    "serverInfo":{"name":"mj-agents","version":env!("CARGO_PKG_VERSION")},
                    "instructions":"Delegate work to Mjolnir sessions in this target. Calls are accepted immediately; results arrive in the parent conversation and are visible in the Sub-agents workspace."
                }),
            ),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({"tools": tool_definitions()})),
            "tools/call" => match call(socket, request.get("params")) {
                Ok((value, is_error)) => rpc_result(
                    id,
                    json!({
                        "content":[{"type":"text","text":serde_json::to_string_pretty(&value)?}],
                        "structuredContent":value,
                        "isError":is_error
                    }),
                ),
                Err(error) => rpc_error(id, -32602, format!("{error:#}")),
            },
            _ => rpc_error(id, -32601, format!("unknown MCP method {method:?}")),
        };
        write_line(&mut output, &response)?;
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
    Ok((
        json!({
            "request_id":request_id,
            "accepted":true,
            "note":"Mjolnir accepted the request. The result will arrive in this conversation; the child is also available in the Sub-agents workspace."
        }),
        false,
    ))
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
                    "working_directory":{"type":"string"},"context":{"type":"string"},"request_key":{"type":"string"},
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

fn write_line(output: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
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
}
