//! The review supervisor's dispatch tool, served over MCP.
//!
//! The extended tier's supervisor decides which specialist lanes are worth
//! running, mid-turn, and launches them by calling one tool:
//! `call_review_subagents`. Doing that through a tool rather than through text
//! is the tier's whole economy -- the supervisor keeps investigating while the
//! lanes it chose run, instead of ending its turn to ask Hel for them.
//!
//! The server is this binary in another mode (`mj worker review-mcp`), started
//! by the supervisor's harness as an ordinary stdio MCP server. It owns no
//! review state: each call is validated here, then forwarded as one JSON line
//! over a Unix socket in the worker root, where the worker records it for the
//! controller to act on. The tool answers as soon as the request is recorded,
//! because a supervisor that blocks inside a tool call cannot be reading the
//! reports its lanes are producing.
//!
//! Hel's MCP servers are hand-rolled JSON-lines loops rather than an SDK
//! (`crate::hel_project_memory::run_mcp_stdio` is the other one), so this file
//! follows that pattern deliberately: one dependency-free `initialize`,
//! `tools/list`, `tools/call` loop.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::lanes::{LaneDispatch, LaneDispatchReply, REVIEW_LANES, validate_dispatch};

/// The MCP server name the supervisor's prompt calls "the private `hel-review`
/// tool".
pub const REVIEW_MCP_SERVER_NAME: &str = "hel-review";
/// The socket, inside the worker's reviewer directory, that carries dispatch
/// requests from the supervisor's tool to the worker.
pub const REVIEW_DISPATCH_SOCKET: &str = "review-dispatch.sock";

/// Serve the review dispatch tool over MCP's JSON-lines stdio transport.
pub fn run_mcp_stdio(socket: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line.context("read MCP request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_json_line(
                    &mut output,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}),
                )?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "initialize" => json_rpc_result(
                id,
                json!({
                    "protocolVersion": request.pointer("/params/protocolVersion").cloned().unwrap_or_else(|| json!("2025-03-26")),
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": REVIEW_MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Launch read-only specialist reviewers for the turn under review. The tool returns immediately; their reports arrive as later messages in this session."
                }),
            ),
            "ping" => json_rpc_result(id, json!({})),
            "tools/list" => json_rpc_result(id, json!({"tools": [tool_definition()]})),
            "tools/call" => match call_tool(socket, request.get("params")) {
                Ok((structured, is_error)) => json_rpc_result(
                    id,
                    json!({
                        "content": [{"type":"text", "text": serde_json::to_string_pretty(&structured)?}],
                        "structuredContent": structured,
                        "isError": is_error
                    }),
                ),
                Err(error) => json_rpc_error(id, -32602, format!("{error:#}")),
            },
            _ => json_rpc_error(id, -32601, format!("unknown MCP method {method:?}")),
        };
        write_json_line(&mut output, &response)?;
    }
    Ok(())
}

fn call_tool(socket: &Path, params: Option<&Value>) -> Result<(Value, bool)> {
    let params = params.context("tools/call is missing params")?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call is missing name")?;
    if name != "call_review_subagents" {
        bail!("unknown review tool {name:?}");
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let dispatch: LaneDispatch = serde_json::from_value(arguments)
        .context("call_review_subagents takes a `reviewers` list")?;
    // Validated here as well as in the worker: a rejected dispatch should read
    // as a tool error the supervisor can correct, not as a silent no-op.
    if let Err(message) = validate_dispatch(&dispatch.reviewers) {
        return Ok((json!({ "error": message }), true));
    }
    let reply = send_dispatch(socket, &dispatch)?;
    if let Some(error) = reply.error {
        return Ok((json!({ "error": error }), true));
    }
    Ok((
        json!({
            "started": reply.started,
            "note": "Reports arrive as later messages in this session. Do not poll or wait for them inside a tool call."
        }),
        false,
    ))
}

/// One request, one line, one reply. The socket lives in the worker root and
/// is only reachable from inside this container.
#[cfg(unix)]
pub fn send_dispatch(socket: &Path, dispatch: &LaneDispatch) -> Result<LaneDispatchReply> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("connect to the review dispatch socket {}", socket.display()))?;
    let mut body = serde_json::to_vec(dispatch)?;
    body.push(b'\n');
    stream
        .write_all(&body)
        .context("send the review dispatch")?;
    stream.flush().ok();
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("read the review dispatch reply")?;
    serde_json::from_str(line.trim()).context("parse the review dispatch reply")
}

/// Hel's workers run on Unix; the tool is compiled everywhere so the CLI and
/// the controller stay one shape, and says plainly where it cannot run.
#[cfg(not(unix))]
pub fn send_dispatch(_socket: &Path, _dispatch: &LaneDispatch) -> Result<LaneDispatchReply> {
    bail!("the review dispatch socket needs a Unix platform")
}

fn tool_definition() -> Value {
    let roster = REVIEW_LANES.iter().map(|lane| lane.id).collect::<Vec<_>>();
    let descriptions = REVIEW_LANES
        .iter()
        .map(|lane| format!("`{}` — {}: {}", lane.id, lane.label, lane.focus))
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "name": "call_review_subagents",
        "description": format!(
            "Launch read-only specialist reviewers for the turn under review. Each request pairs an `agent_type` with a concrete unresolved `hypothesis` the lane can gather evidence for; topical plausibility is not a reason to launch one. The tool returns the started ids immediately and never waits: reports arrive as later messages in this session, and polling inside a tool call cannot receive them.\n\n{descriptions}"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "reviewers": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Nonempty unique reviewer requests, each tied to a concrete hypothesis.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent_type": {
                                "type": "string",
                                "enum": roster,
                                "description": "Specialist reviewer id from the advertised roster."
                            },
                            "hypothesis": {
                                "type": "string",
                                "description": "Concrete unresolved risk this lane should investigate and the evidence it is expected to gather. Topical relevance alone is insufficient."
                            }
                        },
                        "required": ["agent_type", "hypothesis"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["reviewers"],
            "additionalProperties": false
        }
    })
}

fn json_rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn json_rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn write_json_line(output: &mut impl Write, value: &Value) -> Result<()> {
    let mut body = serde_json::to_vec(value)?;
    body.push(b'\n');
    output.write_all(&body).context("write MCP response")?;
    output.flush().context("flush MCP response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tool_schema_names_every_lane_and_demands_a_hypothesis() {
        let schema = tool_definition().to_string();
        for lane in &REVIEW_LANES {
            assert!(schema.contains(lane.id), "the schema offers {}", lane.id);
        }
        assert!(schema.contains("\"hypothesis\""));
        assert!(schema.contains("\"agent_type\""));
        assert!(
            schema.contains("never waits"),
            "the description forbids polling: {schema}"
        );
        assert!(
            !schema.contains("\"quick\""),
            "the quick reviewer is not dispatchable"
        );
    }

    #[test]
    fn a_dispatch_round_trips_through_its_wire_form() {
        let dispatch = LaneDispatch {
            reviewers: vec![crate::hel_review::lanes::ReviewSubagentRequest {
                agent_type: "tests".to_string(),
                hypothesis: "the new test cannot fail for the reason it claims".to_string(),
            }],
        };
        let encoded = serde_json::to_string(&dispatch).unwrap();
        assert_eq!(
            serde_json::from_str::<LaneDispatch>(&encoded).unwrap(),
            dispatch
        );
        let reply = LaneDispatchReply {
            started: vec!["tests".to_string()],
            error: None,
        };
        let encoded = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            serde_json::from_str::<LaneDispatchReply>(&encoded).unwrap(),
            reply
        );
    }

    #[test]
    fn an_invalid_dispatch_is_a_tool_error_the_supervisor_can_correct() {
        let socket = std::path::PathBuf::from("/nonexistent/review-dispatch.sock");
        let params = json!({
            "name": "call_review_subagents",
            "arguments": {"reviewers": [{"agent_type": "tests", "hypothesis": "  "}]}
        });
        let (result, is_error) = call_tool(&socket, Some(&params)).expect("validation answers");
        assert!(is_error);
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|error| error.contains("nonempty concrete hypothesis")),
            "unexpected result {result}"
        );

        let params = json!({
            "name": "call_review_subagents",
            "arguments": {"reviewers": []}
        });
        let (result, is_error) = call_tool(&socket, Some(&params)).expect("validation answers");
        assert!(is_error);
        assert!(result["error"].as_str().unwrap().contains("at least one"));
    }

    #[test]
    fn an_unknown_tool_is_refused() {
        let socket = std::path::PathBuf::from("/nonexistent/review-dispatch.sock");
        let params = json!({"name": "memory_write", "arguments": {}});
        let error = call_tool(&socket, Some(&params)).expect_err("only one tool is served");
        assert!(format!("{error:#}").contains("unknown review tool"));
    }
}
