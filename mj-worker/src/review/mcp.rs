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
            reviewers: vec![mj_core::review::lanes::ReviewSubagentRequest {
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
            "name": "spawn_specialist",
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
            "name": "spawn_specialist",
            "arguments": {"reviewers": []}
        });
        let (result, is_error) = call_tool(&socket, Some(&params)).expect("validation answers");
        assert!(is_error);
        assert!(result["error"].as_str().unwrap().contains("at least one"));
    }

    #[test]
    fn an_unknown_tool_is_refused() {
        let socket = std::path::PathBuf::from("/nonexistent/review-dispatch.sock");
        let params = json!({"name": "write", "arguments": {}});
        let error = call_tool(&socket, Some(&params)).expect_err("only one tool is served");
        assert!(format!("{error:#}").contains("unknown review tool"));
    }
}
