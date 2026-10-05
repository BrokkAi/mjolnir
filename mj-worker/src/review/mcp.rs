//! The review supervisor's dispatch tool, served over MCP.
//!
//! The extended tier's supervisor decides which specialist lanes are worth
//! running, mid-turn, and launches them by calling one tool:
//! `spawn_specialist`. Doing that through a tool rather than through text
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
//! The JSON-RPC loop is the worker's shared `crate::mcp_stdio::serve`.

use mj_core::review::mcp::REVIEW_MCP_SERVER_NAME;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use mj_review::lanes::{LaneDispatch, LaneDispatchReply, REVIEW_LANES, validate_dispatch};

/// Serve the review dispatch tool over MCP's JSON-lines stdio transport.
pub fn run_mcp_stdio(socket: &Path) -> Result<()> {
    let socket = socket.to_path_buf();
    crate::mcp_stdio::serve(
        std::io::stdin().lock(),
        std::io::stdout(),
        crate::mcp_stdio::McpServer {
            name: REVIEW_MCP_SERVER_NAME,
            instructions: "Launch read-only specialist reviewers for the turn under review. The tool returns immediately; their reports arrive as later messages in this session.",
            tools: vec![tool_definition()],
            dispatch: crate::mcp_stdio::Dispatch::Sequential,
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            // This tool returns at once, so it has no progress to report.
            call: move |params: Option<&Value>, _: &crate::mcp_stdio::Progress| {
                call_tool(&socket, params)
            },
        },
    )
}

fn call_tool(socket: &Path, params: Option<&Value>) -> Result<(Value, bool)> {
    let params = params.context("tools/call is missing params")?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call is missing name")?;
    if name != "spawn_specialist" {
        bail!("unknown review tool {name:?}");
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let dispatch: LaneDispatch =
        serde_json::from_value(arguments).context("spawn_specialist takes a `reviewers` list")?;
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
pub fn send_dispatch(socket: &Path, dispatch: &LaneDispatch) -> Result<LaneDispatchReply> {
    crate::mcp_stdio::socket_request(socket, dispatch, "review dispatch", DISPATCH_REPLY_TIMEOUT)?
        .with_context(|| {
            format!(
                "the review dispatch socket did not answer within {}s",
                DISPATCH_REPLY_TIMEOUT.as_secs()
            )
        })
}

/// The worker answers a dispatch as soon as it has queued the lanes, so a
/// reply that takes longer than this means the worker is not serving.
const DISPATCH_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn tool_definition() -> Value {
    let roster = REVIEW_LANES.iter().map(|lane| lane.id).collect::<Vec<_>>();
    let descriptions = REVIEW_LANES
        .iter()
        .map(|lane| format!("`{}` — {}: {}", lane.id, lane.label, lane.focus))
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "name": "spawn_specialist",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};

    #[cfg(unix)]
    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    #[cfg(unix)]
    impl std::io::Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn tools_call_dispatches_over_the_worker_socket_and_returns_invalid_input_to_the_client() {
        // MCP's documented JSON-RPC tools/call envelope carries one valid and one invalid call.
        use std::os::unix::net::UnixListener;

        let directory = tempfile::tempdir().expect("temporary socket directory");
        let socket = directory.path().join("review.sock");
        let listener = UnixListener::bind(&socket).expect("bind worker dispatch socket");
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept valid dispatch");
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream
                .read_line(&mut request)
                .expect("read dispatch request");
            let dispatch: LaneDispatch =
                serde_json::from_str(request.trim()).expect("decode worker dispatch");
            let reply = LaneDispatchReply {
                started: vec!["reviewer-1".into()],
                error: None,
            };
            let mut stream = stream.into_inner();
            serde_json::to_writer(&mut stream, &reply).expect("write worker reply");
            stream.write_all(b"\n").expect("terminate worker reply");
            dispatch
        });

        let requests = concat!(
            r#"{"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"spawn_specialist","arguments":{"reviewers":[{"agent_type":"control_flow","hypothesis":"A retry can duplicate a checkpoint operation."}]}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"spawn_specialist","arguments":{"reviewers":[]}}}"#,
            "\n",
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        crate::mcp_stdio::serve(
            requests.as_bytes(),
            SharedWriter(Arc::clone(&output)),
            crate::mcp_stdio::McpServer {
                name: REVIEW_MCP_SERVER_NAME,
                instructions: "test review dispatch",
                tools: vec![tool_definition()],
                dispatch: crate::mcp_stdio::Dispatch::Sequential,
                progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
                call: move |params: Option<&Value>, _: &crate::mcp_stdio::Progress| {
                    call_tool(&socket, params)
                },
            },
        )
        .expect("serve JSON-RPC calls");

        let dispatch = worker.join().expect("worker socket handler");
        assert_eq!(
            dispatch,
            LaneDispatch {
                reviewers: vec![mj_review::lanes::ReviewSubagentRequest {
                    agent_type: "control_flow".into(),
                    hypothesis: "A retry can duplicate a checkpoint operation.".into(),
                }],
            }
        );

        let responses = String::from_utf8(output.lock().unwrap().clone())
            .expect("JSON-RPC output is UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("valid JSON response"))
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0],
            json!({
                "jsonrpc": "2.0",
                "id": 17,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "{\n  \"started\": [\n    \"reviewer-1\"\n  ],\n  \"note\": \"Reports arrive as later messages in this session. Do not poll or wait for them inside a tool call.\"\n}"
                    }],
                    "structuredContent": {
                        "started": ["reviewer-1"],
                        "note": "Reports arrive as later messages in this session. Do not poll or wait for them inside a tool call."
                    },
                    "isError": false
                }
            })
        );
        assert_eq!(
            responses[1]["result"]["isError"], true,
            "an invalid reviewer list is reported as a correctable MCP tool error"
        );
        assert_eq!(
            responses[1]["result"]["structuredContent"]["error"],
            "reviewers must contain at least one reviewer request"
        );
    }

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
}
