//! Mjolnir-owned delegation tools for Claude and Codex parent sessions.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use mj_core::config::HarnessKind;
use mj_core::subagent::{
    FileSourceRanges, SubagentMcpRole, SubagentToolAction, SubagentToolRequest,
};

/// Server instructions stating the spawn/wait contract: results reach the
/// model only as the `wait` tool call's own answer, never as a push.
const SERVER_INSTRUCTIONS: &str = "Delegate work to Mjolnir child sessions in this target. spawn starts a child and returns its child_session_id immediately; the child runs independently while you continue other work. Collect a child's result only by calling wait, which blocks until the named children finish their current turn or the timeout. Every wait answers: status complete means the children finished and their reports are in output; status still_running means the timeout came first, which is not a failure - call wait again with the children still running. A child may take longer than any single wait. Every wait call costs you a request that carries your whole context, so call wait once with every child you are waiting for and the largest timeout you can afford rather than polling. Use children for context-heavy work: exploration, census, suite runs and mechanical implementation slices. Give each child files excerpts and one question or one bounded slice. A child's report is short and names files in its report_dir; read those files for details instead of asking the child to repeat them, and do not redo work you delegated. The user can see every child in the Sub-agents workspace.";

/// A child's server instructions: its report reaches the parent only through
/// `handback`, which is the rule Claude Code's own subagents follow.
const CHILD_INSTRUCTIONS: &str = concat!(
    "You are a Mjolnir sub-agent working for another session. Deliver your report for each task by calling handback once, as your last action. The session that started you reads that report, not the rest of this conversation. If you need a decision from it, hand back your question and stop; its answer arrives as your next prompt. Your report directory is named in your first prompt. ",
    // Kept in step with `mj_core::subagent::HANDBACK_REPORT_RULES` by a test.
    "Your report is what the parent reads, and it is at most 4,000 characters: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Write anything longer (logs, tables, full findings) to files in your report directory and list their paths in the report; never put it in the report itself. Fix mechanical errors yourself (compile errors, lint and format findings, test failures your own change caused) and list each fix in one line of your report. Hand back a question and stop when the fix needs a design choice, touches a file you were not given, changes a persisted identity, a public contract or an epoch, or the failure also occurs on the base commit."
);

/// A child's advice when Mjolnir has not confirmed its report. Calling again
/// is safe: a report that did arrive is refused as already delivered.
const HANDBACK_UNANSWERED_ADVICE: &str = "It may still be recorded; call handback again with the same report. An answer that it was already delivered means the first call arrived.";

/// What the model is told when Mjolnir has not answered a request: it may
/// still run, so repeating it blindly could spawn a second child or send the
/// same input twice. `list_agents` shows what actually happened.
const UNANSWERED_ADVICE: &str =
    "It may still run; call list_agents to see the children's state before repeating this call.";

/// What to do about a request Mjolnir has not answered, for this action.
fn unanswered_advice(action: &SubagentToolAction) -> &'static str {
    match action {
        SubagentToolAction::Handback { .. } => HANDBACK_UNANSWERED_ADVICE,
        _ => UNANSWERED_ADVICE,
    }
}

/// The degraded answer when the daemon has not completed the request within
/// the socket ceiling. The request stays queued; the model must check on it
/// itself, because nothing is ever pushed into its conversation.
fn pending_reply(request_id: &str, action: &SubagentToolAction) -> Value {
    json!({
        "request_id":request_id,
        "accepted":true,
        "note":format!("Mjolnir has not answered this request yet. {}", unanswered_advice(action))
    })
}

/// How long any action other than `wait` may take to be answered. The daemon
/// must notice the queued request, run the action (a spawn may provision a
/// child session) and complete it back to the worker.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// The answer to a `close` Mjolnir has not confirmed within the call's budget.
///
/// A close is finished when the child's process tree is gone, which is later
/// than the moment the request was taken: it cancels whatever owned the session,
/// checkpoints, seals the relay and tears the target down. Calling that
/// "accepted" told a parent nothing it could act on, and a parent that then
/// spawned the replacement child stacked both process trees inside one
/// container until it ran out of process slots (#1087). So say what is true —
/// the close may still be running — and name the one tool that can observe the
/// child being gone.
fn still_closing_reply(request_id: &str, child_session_id: &str, reason: &str) -> Value {
    json!({
        "request_id":request_id,
        "child_session_id":child_session_id,
        "closed":false,
        "status":"still_closing",
        "note":reason,
        "next_action":"Call wait with this child_session_id. The close is finished when that wait answers status complete with this child's state \"stopped\"; a child that is still being torn down reports state \"stopping\". Do not spawn a replacement child before then."
    })
}

/// Slack added to a `wait` call's own timeout. The worker answers a `wait` at
/// the caller's deadline itself, so this only covers the hop back from the
/// worker; it is not the timer the answer depends on.
const WAIT_REPLY_GRACE: Duration = Duration::from_secs(30);

/// How long the shim waits for the worker to answer `action`. A `wait` blocks
/// for as long as the caller asked plus grace; everything else is bounded by
/// [`REPLY_TIMEOUT`].
fn reply_timeout(action: &SubagentToolAction) -> Duration {
    match action {
        SubagentToolAction::WaitAgents {
            timeout_seconds, ..
        } => mj_core::subagent::subagent_wait_timeout(*timeout_seconds) + WAIT_REPLY_GRACE,
        _ => REPLY_TIMEOUT,
    }
}

/// The answer when the worker itself never replied. For a `wait` this is the
/// same "still running, ask again" answer the worker and the daemon give, so
/// the model reads one rule whatever went slow; for a `close` it is the "still
/// closing" answer, because `wait` can observe how the close ends. For anything
/// else it is a tool error, because there is no honest answer to give.
fn unanswered_reply(
    request_id: &str,
    action: &SubagentToolAction,
    waited: Duration,
) -> (Value, bool) {
    if let SubagentToolAction::CloseAgent { child_session_id } = action {
        return (
            still_closing_reply(
                request_id,
                child_session_id,
                &format!(
                    "Mjolnir did not confirm this close within {} seconds. The close may still be running, so this child is not known to be gone.",
                    waited.as_secs()
                ),
            ),
            false,
        );
    }
    if let SubagentToolAction::WaitAgents {
        child_session_ids, ..
    } = action
    {
        let mut payload = mj_core::subagent::still_running_payload(
            child_session_ids,
            waited.as_secs(),
            Some(
                "Mjolnir did not answer this wait in time, so these children's state is unknown \
                 rather than observed. They are still running; call wait again to collect them.",
            ),
        );
        if let Some(object) = payload.as_object_mut() {
            object.insert("request_id".into(), json!(request_id));
        }
        return (payload, false);
    }
    (
        json!({
            "request_id": request_id,
            "error": format!(
                "Mjolnir did not answer this request within {} seconds. {}",
                waited.as_secs(),
                unanswered_advice(action)
            )
        }),
        true,
    )
}

/// What the model reads for one answer. The daemon and the worker both put the
/// answer's own JSON into `SubagentToolResult.message`, so handing the envelope
/// back would leave the model parsing JSON out of a string inside a wrapper.
/// Unwrap it, keeping `request_id` alongside the answer.
fn model_facing(request_id: &str, result: &Value) -> Value {
    let Some(message) = result.get("message").and_then(Value::as_str) else {
        return result.clone();
    };
    match serde_json::from_str::<Value>(message) {
        Ok(Value::Object(mut payload)) => {
            payload
                .entry("request_id".to_owned())
                .or_insert_with(|| json!(request_id));
            Value::Object(payload)
        }
        _ => result.clone(),
    }
}

/// What a progress notification says while a `wait` is open. A client that
/// shows it to a person, or to a model, should learn something from it.
fn wait_progress_message(action: &SubagentToolAction, elapsed: Duration) -> Option<String> {
    let SubagentToolAction::WaitAgents {
        child_session_ids, ..
    } = action
    else {
        return None;
    };
    Some(format!(
        "waiting for {} child session(s) to finish their turn: {}; {}s elapsed",
        child_session_ids.len(),
        child_session_ids.join(", "),
        elapsed.as_secs()
    ))
}

pub fn run_mcp_stdio(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
) -> Result<()> {
    let stdin = std::io::stdin();
    run(stdin.lock(), std::io::stdout(), socket, harness, role)
}

/// Serve MCP over `reader`/`writer` against the worker `socket`. Calls are
/// dispatched concurrently, each on its own socket connection, so a long
/// `wait` never blocks a cheap `list_agents` queued after it. `harness` is the
/// parent's own harness, whose client decides how long one call may stay open.
/// `role` picks the tool set: a parent delegates, a child hands back.
fn run<R: BufRead, W: Write + Send + Sync + 'static>(
    reader: R,
    writer: W,
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
) -> Result<()> {
    let socket = socket.to_path_buf();
    let (instructions, tools) = match role {
        SubagentMcpRole::Parent => (SERVER_INSTRUCTIONS, tool_definitions(harness)),
        SubagentMcpRole::Child => (CHILD_INSTRUCTIONS, child_tool_definitions()),
    };
    crate::mcp_stdio::serve(
        reader,
        writer,
        crate::mcp_stdio::McpServer {
            name: mj_core::subagent::SUBAGENT_MCP_SERVER,
            instructions,
            tools,
            dispatch: crate::mcp_stdio::Dispatch::Concurrent,
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            call: move |params: Option<&Value>, progress: &crate::mcp_stdio::Progress| {
                call(&socket, harness, role, params, progress)
            },
        },
    )
}

#[derive(Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

/// Strict, like the schema it answers to: a model still sending the removed
/// `request_key` is told so instead of having it silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
}

#[derive(Deserialize)]
struct ChildArgs {
    child_session_id: String,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandbackArgs {
    message: String,
}

#[derive(Deserialize)]
struct WaitArgs {
    child_session_ids: Vec<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    return_when: mj_core::subagent::ReturnWhen,
}

fn call(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
) -> Result<(Value, bool)> {
    call_with_budget(socket, harness, role, params, progress, reply_timeout)
}

/// Answer one tool call, waiting for the worker as long as `budget` allows
/// for the call's action.
fn call_with_budget(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
    budget: impl Fn(&SubagentToolAction) -> Duration,
) -> Result<(Value, bool)> {
    let params: CallParams = serde_json::from_value(params.cloned().context("missing params")?)?;
    // Each role answers only the tools it lists.
    match (role, params.name.as_str()) {
        (SubagentMcpRole::Child, "handback") | (SubagentMcpRole::Parent, _) => {}
        (SubagentMcpRole::Child, other) => {
            bail!("unknown sub-agent tool {other:?}; a sub-agent can only call handback")
        }
    }
    let action = match params.name.as_str() {
        "list_profiles" => SubagentToolAction::ListProfiles,
        "spawn" => {
            let args: SpawnArgs = serde_json::from_value(params.arguments)?;
            SubagentToolAction::Spawn {
                task_name: args.task_name,
                instructions: args.instructions,
                profile_id: args.profile_id,
                model: args.model,
                effort: args.effort,
                working_directory: args.working_directory,
                context: args.context,
                files: args.files,
            }
        }
        "list_agents" => SubagentToolAction::ListAgents,
        "send_input" => {
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            let message = args.message.context("send_input requires message")?;
            SubagentToolAction::SendInput {
                child_session_id: args.child_session_id,
                message,
            }
        }
        "wait" => {
            let args: WaitArgs = serde_json::from_value(params.arguments)?;
            // Apply the harness's own ceiling once, here, so the worker and the
            // daemon work to the same deadline the caller will be answered at.
            SubagentToolAction::WaitAgents {
                child_session_ids: args.child_session_ids,
                timeout_seconds: Some(
                    mj_core::subagent::subagent_wait_timeout_for(harness, args.timeout_seconds)
                        .as_secs(),
                ),
                return_when: args.return_when,
            }
        }
        "interrupt" => {
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            SubagentToolAction::InterruptAgent {
                child_session_id: args.child_session_id,
            }
        }
        "close" => {
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            SubagentToolAction::CloseAgent {
                child_session_id: args.child_session_id,
            }
        }
        "handback" if role == SubagentMcpRole::Child => {
            let args: HandbackArgs = serde_json::from_value(params.arguments)?;
            SubagentToolAction::Handback {
                message: args.message,
            }
        }
        other => bail!("unknown sub-agent tool {other:?}"),
    };
    // Each call is its own request. The id keeps the worker's queue and the
    // daemon from running one request twice; it is never the model's to set.
    let request_id = mj_core::state::new_session_id()?;
    let timeout = budget(&action);
    let request = SubagentToolRequest {
        request_id: request_id.clone(),
        created_at_ms: mj_core::clock::epoch_millis(),
        action,
    };
    let Some(reply) = send(socket, &request, timeout, progress)? else {
        return Ok(unanswered_reply(&request_id, &request.action, timeout));
    };
    if let Some(result) = reply.get("result").filter(|value| !value.is_null()) {
        return Ok((
            model_facing(&request_id, result),
            result
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ));
    }
    if let SubagentToolAction::CloseAgent { child_session_id } = &request.action {
        return Ok((
            still_closing_reply(
                &request_id,
                child_session_id,
                "Mjolnir took this close but has not confirmed it, so this child is not known to be gone.",
            ),
            false,
        ));
    }
    Ok((pending_reply(&request_id, &request.action), false))
}

/// Send the request and wait for the worker's answer, reporting progress while
/// it is outstanding. `None` means the worker did not answer within `timeout`.
///
/// The exchange runs on its own thread so this one can keep reporting: a
/// silent call is what makes a harness abandon a long `wait`, and the answer
/// itself may legitimately be a long time coming.
fn send(
    socket: &Path,
    request: &SubagentToolRequest,
    timeout: Duration,
    progress: &crate::mcp_stdio::Progress,
) -> Result<Option<Value>> {
    let started = std::time::Instant::now();
    let (sender, receiver) = std::sync::mpsc::channel();
    let exchange = {
        let socket = socket.to_path_buf();
        let request = request.clone();
        std::thread::spawn(move || {
            let _ = sender.send(crate::mcp_stdio::socket_request(
                &socket,
                &request,
                "sub-agent",
                timeout,
            ));
        })
    };
    let total = match &request.action {
        SubagentToolAction::WaitAgents {
            timeout_seconds, ..
        } => Some(mj_core::subagent::subagent_wait_timeout(*timeout_seconds).as_secs()),
        _ => None,
    };
    let answer = loop {
        match receiver.recv_timeout(progress.interval()) {
            Ok(answer) => break answer,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Some(message) = wait_progress_message(&request.action, started.elapsed()) {
                    progress.notify(started.elapsed().as_secs(), total, &message);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("the sub-agent socket thread ended without a result")
            }
        }
    };
    if exchange.join().is_err() {
        anyhow::bail!("the sub-agent socket thread panicked");
    }
    answer
}

fn tool_definitions(harness: Option<HarnessKind>) -> Vec<Value> {
    let ceiling = mj_core::subagent::max_wait_seconds_for(harness);
    let default_wait = mj_core::subagent::subagent_wait_timeout_for(harness, None).as_secs();
    let child = json!({"type":"object","properties":{"child_session_id":{"type":"string"}},"required":["child_session_id"],"additionalProperties":false});
    let current = mj_core::subagent::CURRENT_MODEL;
    let model = format!(
        "A model value from list_profiles, or \"{current}\" for this session's own model. Unless profile_id is given, Mjolnir runs the child on the eligible profile that offers this model and has the most quota left (the lower of its 5-hour and weekly remaining)."
    );
    vec![
        tool(
            "list_profiles",
            "List eligible sub-agent profiles and the models and efforts each offers. Profiles that offer the same models are listed once, as the one with the most quota left.",
            json!({"type":"object","additionalProperties":false}),
        ),
        tool(
            "spawn",
            "Start an independent Mjolnir child session in this session's target and filesystem. Use a child for context-heavy work (exploration, census, suite runs, a mechanical implementation slice), and give it files excerpts and one question or one bounded slice. Returns child_session_id and report_dir at once: report_dir is the directory where the child writes the details its short report points to. child_session_id means the child was registered, not that it started. The child starts on its own; collect its result, or the reason it could not start, with wait or list_agents, which report state \"error\" with the reason as output. A child that ends in error cannot be re-prompted; spawn a new one instead.",
            json!({
                "type":"object",
                "properties":{
                    "task_name":{"type":"string"},"instructions":{"type":"string"},
                    "profile_id":{"type":"string","description":"Only to pin one profile; normally omit it and let the model choose the profile."},"model":{"type":"string","description":model},"effort":{"type":"string","description":"An effort the chosen profile offers. Omitted, the child uses this session's effort when the chosen profile offers it, otherwise the harness default."},
                    "working_directory":{"type":"string","description":"Launch directory for the child session on the parent's target. Absolute paths are used as-is; relative paths resolve against the parent session's working directory. The directory must exist; no other restriction applies. Defaults to the parent session's working directory."},"context":{"type":"string"},
                    "files":{"type":"array","description":"Source excerpts to include in the child's first prompt, grouped by file. Each entry names one relative file and a list of one or more one-based, inclusive line ranges to pull from it.","items":{"type":"object","properties":{"file":{"type":"string"},"ranges":{"type":"array","minItems":1,"items":{"type":"object","properties":{"start":{"type":"integer","minimum":1},"end":{"type":"integer","minimum":1}},"required":["start","end"],"additionalProperties":false}}},"required":["file","ranges"],"additionalProperties":false}}
                },
                "required":["task_name","instructions","model"],"additionalProperties":false
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
            &format!(
                "Block until the named child sessions finish their current turn, or until the timeout, whichever comes first. Call wait once with every child you are waiting for and the largest timeout you can afford; every wait call costs you a request with your whole context, so do not poll with short timeouts. Use return_when any when the next step depends on whichever finishes first; it answers as soon as one named child finishes, and the others show finished false. The answer's status field is complete when every named child finished, with its report in that child's output, or still_running when some child had not. output is the short report the child handed back, or its last message when it did not hand one back; report_source says which. The report names files in the child's report_dir for the details. An output longer than {max_output} characters is cut and marked truncated. A child Mjolnir has reminded to hand back its report still reads as running. still_running is not a failure and says nothing about whether the work is going well: call wait again with the children still running, or do other work first and call wait later. wait also follows a child you have closed: while the close runs that child reports state \"stopping\" and is not finished, and it reports state \"stopped\" once it is gone. timeout_seconds defaults to {default_wait}, the most this session allows; a child may run far longer than that, so expect to call wait more than once.",
                max_output = mj_core::subagent::MAX_HANDBACK_CHARS
            ),
            json!({"type":"object","properties":{"child_session_ids":{"type":"array","items":{"type":"string"},"minItems":1},"timeout_seconds":{"type":"integer","minimum":1,"maximum":ceiling},"return_when":{"type":"string","enum":["all","any"],"description":"all (the default) answers once every named child finished; any answers once one of them did."}},"required":["child_session_ids"],"additionalProperties":false}),
        ),
        tool(
            "interrupt",
            "Interrupt the current turn of one child.",
            child.clone(),
        ),
        tool(
            "close",
            "Stop one child session and retain its conversation. Stopping a child is not instant: it checkpoints the child, seals its transcript and tears its process tree down. An answer of closed true means that finished; any other answer, including status still_closing, means the child may still be on its way out. When you are replacing a child, call wait with the closed child's id first and spawn its replacement only once that wait reports the child finished with state \"stopped\" - a child that is still stopping holds its share of this target's processes, and starting the next one on top of it can exhaust them.",
            child,
        ),
    ]
}

/// A child's only tool: its report to the session that started it.
fn child_tool_definitions() -> Vec<Value> {
    vec![tool(
        "handback",
        &format!(
            "Deliver your report to the session that started you. Call it once, as your last action for the task: that session reads this report, not the rest of your conversation. A second call in the same turn is refused. If you need a decision from that session, hand back your question and stop; its answer arrives as your next prompt. {}",
            mj_core::subagent::HANDBACK_REPORT_RULES
        ),
        json!({"type":"object","properties":{"message":{"type":"string","description":"Your report, at most 4,000 characters; details go in files in your report directory."}},"required":["message"],"additionalProperties":false}),
    )]
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
        let wait = tool_definitions(None)
            .into_iter()
            .find(|tool| tool["name"] == "wait")
            .expect("wait definition");
        assert_eq!(
            wait["inputSchema"]["properties"]["timeout_seconds"]["maximum"],
            mj_core::subagent::MAX_WAIT_SECONDS
        );
    }

    #[test]
    fn instructions_collect_results_through_wait_and_never_promise_a_push() {
        assert!(SERVER_INSTRUCTIONS.contains("wait"));
        assert!(
            !SERVER_INSTRUCTIONS.contains("arrive in"),
            "instructions must not promise pushed results: {SERVER_INSTRUCTIONS}"
        );
        // Each wait call resends the parent's whole context, so the parent is
        // told to wait long and once, and to read details from files.
        for needed in [
            "rather than polling",
            "largest timeout",
            "report_dir",
            "do not redo",
        ] {
            assert!(
                SERVER_INSTRUCTIONS.contains(needed),
                "{needed}: {SERVER_INSTRUCTIONS}"
            );
        }
        let wait = tool_definitions(None)
            .into_iter()
            .find(|tool| tool["name"] == "wait")
            .expect("wait definition");
        let description = wait["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("do not poll") && description.contains("return_when any"),
            "{description}"
        );
        assert_eq!(
            wait["inputSchema"]["properties"]["return_when"]["enum"],
            json!(["all", "any"])
        );
    }

    #[test]
    fn a_wait_without_a_timeout_advertises_the_harness_ceiling_as_its_default() {
        for harness in [None, Some(HarnessKind::Codex)] {
            let ceiling = mj_core::subagent::max_wait_seconds_for(harness);
            let wait = tool_definitions(harness)
                .into_iter()
                .find(|tool| tool["name"] == "wait")
                .expect("wait definition");
            let description = wait["description"].as_str().unwrap_or_default();
            assert!(
                description.contains(&format!("defaults to {ceiling},")),
                "{description}"
            );
        }
    }

    #[test]
    fn the_child_is_told_the_same_report_rules_everywhere() {
        assert!(
            CHILD_INSTRUCTIONS.contains(mj_core::subagent::HANDBACK_REPORT_RULES),
            "{CHILD_INSTRUCTIONS}"
        );
        let handback = child_tool_definitions().remove(0);
        assert!(
            handback["description"]
                .as_str()
                .unwrap_or_default()
                .contains(mj_core::subagent::HANDBACK_REPORT_RULES),
            "{handback}"
        );
    }

    #[test]
    fn the_pending_reply_sends_the_model_to_list_agents_before_a_retry() {
        let reply = pending_reply("request-1", &SubagentToolAction::ListAgents);
        assert_eq!(reply["request_id"], "request-1");
        assert_eq!(reply["accepted"], true);
        let note = reply["note"].as_str().expect("note text");
        assert!(
            note.contains("list_agents") && !note.contains("request_key"),
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
            return_when: Default::default(),
        };
        assert_eq!(
            reply_timeout(&wait(Some(7))),
            Duration::from_secs(7) + WAIT_REPLY_GRACE
        );
        assert_eq!(
            reply_timeout(&wait(None)),
            Duration::from_secs(mj_core::subagent::DEFAULT_WAIT_SECONDS) + WAIT_REPLY_GRACE
        );
        assert_eq!(
            reply_timeout(&wait(Some(mj_core::subagent::MAX_WAIT_SECONDS * 2))),
            Duration::from_secs(mj_core::subagent::MAX_WAIT_SECONDS) + WAIT_REPLY_GRACE
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
        let (value, is_error) = call_with_budget(
            &socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name": "list_agents"})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_millis(200),
        )
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
            error.contains("did not answer")
                && error.contains("list_agents")
                && !error.contains("request_key"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unanswered_wait_is_a_still_running_answer_rather_than_a_failure() {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        // Fake worker: take the request and never answer it.
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            std::thread::sleep(Duration::from_secs(2));
            let _ = reader.into_inner().read(&mut [0u8; 1]);
        });

        let (value, is_error) = call_with_budget(
            &socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({
                "name": "wait",
                "arguments": {"child_session_ids": ["c1", "c2"], "timeout_seconds": 5}
            })),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_millis(200),
        )
        .unwrap();

        assert!(
            !is_error,
            "a deadline reached is an answer, not a tool error: {value}"
        );
        assert_eq!(
            value["status"],
            mj_core::subagent::WAIT_STATUS_STILL_RUNNING
        );
        assert_eq!(value["agents"][1]["child_session_id"], "c2");
        assert_eq!(value["agents"][1]["finished"], false);
        assert!(
            !value["request_id"].as_str().unwrap_or_default().is_empty(),
            "{value}"
        );
        let next = value["next_action"].as_str().expect("next_action text");
        assert!(next.contains("Call wait again"), "{next}");
    }

    #[test]
    fn the_models_answer_is_the_payload_itself_not_the_result_envelope() {
        let envelope = json!({
            "request_id": "r-1",
            "completed_at_ms": 7,
            "is_error": false,
            "message": "{\"status\":\"complete\",\"agents\":[]}"
        });
        let value = model_facing("r-1", &envelope);
        assert_eq!(value["status"], "complete");
        assert_eq!(value["request_id"], "r-1");
        assert!(
            value.get("message").is_none(),
            "the payload must not stay wrapped in a stringified message: {value}"
        );

        // A message that is not JSON, such as an error string, is left alone.
        let plain = json!({"request_id":"r-2","is_error":true,"message":"child not found"});
        assert_eq!(model_facing("r-2", &plain), plain);
    }

    #[test]
    fn a_child_is_offered_only_handback() {
        let tools = child_tool_definitions();
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(names, ["handback"]);
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            json!(["message"]),
            "{}",
            tools[0]
        );
        assert!(
            tool_definitions(None)
                .iter()
                .all(|tool| tool["name"] != "handback"),
            "a parent has no report to hand back"
        );
    }

    /// A Claude profile is staged with an allow rule for each tool in
    /// `tool_names`, so a tool listed here and missing there would make Claude
    /// ask a person before running it (R11-1).
    #[test]
    fn each_role_lists_exactly_the_tools_its_harness_is_allowed() {
        let names = |tools: Vec<Value>| {
            tools
                .iter()
                .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(tool_definitions(None)),
            SubagentMcpRole::Parent.tool_names()
        );
        assert_eq!(
            names(child_tool_definitions()),
            SubagentMcpRole::Child.tool_names()
        );
    }

    #[test]
    fn each_role_refuses_the_other_roles_tools() {
        let progress = crate::mcp_stdio::Progress::silent(Duration::from_millis(50));
        let socket = Path::new("/nonexistent/subagents.sock");
        let child = call_with_budget(
            socket,
            None,
            SubagentMcpRole::Child,
            Some(&json!({"name": "spawn", "arguments": {"task_name": "t", "instructions": "i"}})),
            &progress,
            |_| Duration::from_millis(50),
        )
        .expect_err("a child cannot spawn");
        assert!(
            format!("{child:#}").contains("only call handback"),
            "{child:#}"
        );
        let parent = call_with_budget(
            socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name": "handback", "arguments": {"message": "done"}})),
            &progress,
            |_| Duration::from_millis(50),
        )
        .expect_err("a parent cannot hand back");
        assert!(
            format!("{parent:#}").contains("unknown sub-agent tool"),
            "{parent:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_sends_its_report_as_a_handback_request() {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (sent, received) = std::sync::mpsc::channel::<Value>();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            let request_id = request["request_id"].clone();
            sent.send(request).unwrap();
            let reply = json!({"accepted": true, "result": {
                "request_id": request_id, "completed_at_ms": 1, "is_error": false,
                "message": "{\"delivered\":true}"
            }});
            let mut body = serde_json::to_vec(&reply).unwrap();
            body.push(b'\n');
            let mut stream = reader.into_inner();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            let _ = stream.read(&mut [0u8; 1]);
        });
        let (value, is_error) = call_with_budget(
            &socket,
            None,
            SubagentMcpRole::Child,
            Some(&json!({"name": "handback", "arguments": {"message": "the report"}})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_secs(5),
        )
        .unwrap();
        assert!(!is_error, "{value}");
        assert_eq!(value["delivered"], true, "{value}");
        let request = received.recv().unwrap();
        assert_eq!(
            request["action"],
            json!({"action": "handback", "params": {"message": "the report"}})
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_wait_for_any_child_carries_return_when_and_the_harness_default() {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (sent, received) = std::sync::mpsc::channel::<Value>();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(line.trim()).unwrap();
            let request_id = request["request_id"].clone();
            sent.send(request).unwrap();
            let reply = json!({"accepted": true, "result": {
                "request_id": request_id, "completed_at_ms": 1, "is_error": false,
                "message": "{\"status\":\"still_running\",\"agents\":[]}"
            }});
            let mut body = serde_json::to_vec(&reply).unwrap();
            body.push(b'\n');
            let mut stream = reader.into_inner();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            let _ = stream.read(&mut [0u8; 1]);
        });
        let (value, is_error) = call_with_budget(
            &socket,
            Some(HarnessKind::Codex),
            SubagentMcpRole::Parent,
            Some(&json!({"name": "wait", "arguments": {
                "child_session_ids": ["c1", "c2"], "return_when": "any"
            }})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_secs(5),
        )
        .unwrap();
        assert!(!is_error, "{value}");
        let request = received.recv().unwrap();
        assert_eq!(
            request["action"],
            json!({"action": "wait_agents", "params": {
                "child_session_ids": ["c1", "c2"],
                "timeout_seconds": mj_core::subagent::MAX_CODEX_WAIT_SECONDS,
                "return_when": "any"
            }})
        );
    }

    #[test]
    fn an_unanswered_handback_tells_the_child_to_call_again() {
        let (value, is_error) = unanswered_reply(
            "request-1",
            &SubagentToolAction::Handback {
                message: "r".into(),
            },
            Duration::from_secs(120),
        );
        assert!(is_error, "{value}");
        let error = value["error"].as_str().expect("error text");
        assert!(
            error.contains("call handback again") && !error.contains("list_agents"),
            "{error}"
        );
    }

    #[test]
    fn spawn_takes_no_caller_chosen_key() {
        let spawn = tool_definitions(None)
            .into_iter()
            .find(|tool| tool["name"] == "spawn")
            .expect("spawn definition");
        assert!(
            spawn["inputSchema"]["properties"]
                .get("request_key")
                .is_none(),
            "{spawn}"
        );
        assert!(
            !spawn["description"]
                .as_str()
                .unwrap_or_default()
                .contains("request_key"),
            "{spawn}"
        );
        let refused = call_with_budget(
            Path::new("/nonexistent/subagents.sock"),
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name": "spawn", "arguments": {
                "task_name": "t", "instructions": "i", "request_key": "k"
            }})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_millis(50),
        )
        .expect_err("a spawn naming request_key must be refused");
        assert!(
            format!("{refused:#}").contains("unknown field `request_key`"),
            "{refused:#}"
        );
    }

    #[test]
    fn spawn_documents_how_a_working_directory_resolves() {
        let spawn = tool_definitions(None)
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
        use std::sync::mpsc;

        // Observe output after `run` consumes the writer, and announce the
        // response the fake worker is waiting for. The writer is the one place
        // that knows a response has been written, so releasing the slow call
        // from here orders the two responses by events: no sleep, and no
        // assumption about how fast either thread runs.
        struct SharedWriter {
            written: Arc<Mutex<Vec<u8>>>,
            cheap_call_answered: mpsc::Sender<()>,
        }

        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.written
                    .lock()
                    .expect("shared writer poisoned")
                    .extend_from_slice(buf);
                // `write_line` writes one whole response per call.
                if String::from_utf8_lossy(buf).contains("\"id\":2") {
                    let _ = self.cheap_call_answered.send(());
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        let (cheap_call_answered, answered) = mpsc::channel::<()>();
        // One receiver, taken by whichever connection carries the `wait`.
        let answered = Arc::new(Mutex::new(answered));

        // Fake worker: reply to any non-wait call at once, and hold the
        // `wait_agents` reply until the cheap call has been answered. Each
        // accepted connection is served on its own thread so the held reply
        // cannot serialize the two calls at the socket layer.
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let answered = Arc::clone(&answered);
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
                        answered
                            .lock()
                            .expect("handshake receiver poisoned")
                            .recv()
                            .expect("the cheap call must be answered while the wait is open");
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

        // The slow `wait` is sent first, the cheap `list_agents` second. Only
        // concurrent dispatch can answer the cheap call first, because the
        // `wait`'s reply is released by that answer being written. A dispatcher
        // that ran the calls in order would answer the `wait` first — nothing
        // releases it, so it would fall back to its own budget — and the two
        // responses would come back the other way round.
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"wait","arguments":{"child_session_ids":["c1"],"timeout_seconds":1}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_agents"}}),
        );
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        run(
            input.as_bytes(),
            SharedWriter {
                written: Arc::clone(&buffer),
                cheap_call_answered,
            },
            &socket,
            None,
            SubagentMcpRole::Parent,
        )
        .unwrap();

        let written = buffer.lock().unwrap();
        let text = std::str::from_utf8(&written).unwrap();
        // Progress notifications carry no id; only the two responses do.
        let answered_ids = text
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|response| response["id"].as_u64())
            .collect::<Vec<_>>();
        assert_eq!(
            answered_ids,
            vec![2, 1],
            "the cheap list_agents response must be written before the slow wait: {text}"
        );
    }

    /// A close is not finished when it is taken: the child is checkpointed, its
    /// transcript sealed and its process tree torn down, and until that ends the
    /// child still holds its share of the target's processes. Answering
    /// "accepted" let a parent spawn the replacement child on top of the one
    /// leaving, which exhausted a container's process slots (#1087). An
    /// unconfirmed close must say it is still closing and name the tool that can
    /// observe the child being gone.
    #[cfg(unix)]
    #[test]
    fn an_unconfirmed_close_says_it_is_still_closing_rather_than_accepted() {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        // Fake worker: take the close and never confirm it.
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            std::thread::sleep(Duration::from_secs(2));
            let _ = reader.into_inner().read(&mut [0u8; 1]);
        });

        let (value, is_error) = call_with_budget(
            &socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name":"close","arguments":{"child_session_id":"c1"}})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_millis(200),
        )
        .unwrap();

        assert!(
            !is_error,
            "an unconfirmed close is an answer with a next step, not a tool error: {value}"
        );
        assert_eq!(value["status"], "still_closing", "{value}");
        assert_eq!(value["closed"], false, "{value}");
        assert_eq!(value["child_session_id"], "c1", "{value}");
        let next = value["next_action"].as_str().expect("next_action text");
        assert!(
            next.contains("wait") && next.contains("stopped") && next.contains("replacement"),
            "{next}"
        );
    }

    /// A close only answers once Mjolnir has confirmed it, so the confirmed
    /// answer is the one a parent may act on. Nothing may manufacture an
    /// "accepted" answer while the close is still outstanding.
    #[cfg(unix)]
    #[test]
    fn a_close_answers_only_once_mjolnir_confirms_the_child_is_gone() {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;
        use std::sync::mpsc;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        let (child_is_gone, gone) = mpsc::channel::<()>();
        // Fake worker: hold the close until the test says the child is gone.
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            gone.recv().expect("the close must stay open until then");
            let reply = json!({
                "accepted": true,
                "result": {"is_error": false, "message": "{\"child_session_id\":\"c1\",\"closed\":true}"}
            });
            let mut body = serde_json::to_vec(&reply).unwrap();
            body.push(b'\n');
            let mut stream = reader.into_inner();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            let _ = stream.read(&mut [0u8; 1]);
        });

        let (answered, answer) = mpsc::channel();
        let closing = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                let reply = call_with_budget(
                    &socket,
                    None,
                    SubagentMcpRole::Parent,
                    Some(&json!({"name":"close","arguments":{"child_session_id":"c1"}})),
                    &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
                    |_| Duration::from_secs(30),
                );
                let _ = answered.send(());
                reply
            })
        };

        // The close cannot have been answered yet: the only reply it can get is
        // the one the fake worker is still holding.
        assert!(
            matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the close answered before the child was gone"
        );
        child_is_gone.send(()).unwrap();
        let (value, is_error) = closing.join().expect("close thread").unwrap();
        assert!(!is_error, "{value}");
        assert_eq!(value["closed"], true, "{value}");
    }

    /// The parent model has to learn the rule from the tools themselves: the
    /// close is finished only when a wait reports the child stopped, and a
    /// replacement child waits for that.
    #[test]
    fn close_directs_the_model_to_wait_for_a_stopped_child_before_replacing_it() {
        let definitions = tool_definitions(None);
        let description = |name: &str| {
            definitions
                .iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["description"].as_str())
                .unwrap_or_else(|| panic!("{name} definition"))
                .to_owned()
        };

        let close = description("close");
        assert!(close.contains("wait"), "{close}");
        assert!(close.contains("\"stopped\""), "{close}");
        assert!(
            close.contains("replacement"),
            "the close must say what to do before spawning the next child: {close}"
        );

        // And `wait` has to advertise that it follows a close at all, or the
        // advice above names a tool that says nothing about closing children.
        let wait = description("wait");
        assert!(wait.contains("closed"), "{wait}");
        assert!(
            wait.contains("\"stopping\"") && wait.contains("\"stopped\""),
            "{wait}"
        );
    }
}
