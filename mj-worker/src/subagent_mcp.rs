//! Mjolnir-owned delegation tools for Claude and Codex parent sessions.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mj_core::config::HarnessKind;
use mj_core::subagent::{SubagentMcpRole, SubagentToolAction, SubagentToolRequest};

/// Delegation policy is delivered through initialization, never a tool description.
const DELEGATION_ROUTING: &str = "Delegate broad exploration, substantial reading, test suites, lint and format runs, and independent investigation before gathering that context yourself; keep only known-file lookups and small checks without a build. Implement through children by default: split the agreed design into independent slices, each with a spec, ownership boundaries and tests, and dispatch them together. Keep a slice only when it needs your whole context, briefing would cost more than doing it, or a child already failed it once; say why in one sentence. You own design, integrated review, the commit and final acceptance. Collect reports with wait or list_agents; nothing is pushed to you, and a prompt saying children finished has no child output. Read the short handback and decisive files in report_dir instead of duplicating work or importing every log. Re-task a wrong or incomplete handback once with the correction and failing evidence; take over a second failure and note it in your report. In instructions, give the outcome, constraints, ownership boundaries and required evidence, with pointers to files, symbols, line ranges and earlier reports. Children share your target and filesystem, not your conversation; named files are pointers, not a whitelist. Children report test names, reasons and log paths; rerun only to reproduce a reported failure, never to repeat a green one. Children can collect profiles and timings; decide performance design yourself. Do not duplicate work children are doing.";
const MAILBOX_ROUTING: &str = " Use send_message for a note delivered at a running child's next tool boundary or to wake an idle child; it does not cancel the turn.";
const INPUT_ROUTING: &str = " Use send_input for new turns. Before spawning, give follow-up work through send_input to an idle child with useful context; spawn for independent work or misleading context.";

/// Reports reach the model through `wait`; a prompt can tell the parent one is
/// ready without including the child's output.
static SERVER_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{DELEGATION_ROUTING}{MAILBOX_ROUTING}{INPUT_ROUTING} Finished children are parked and hold no processes; send_input resumes one with its conversation intact. Close children you no longer need, including failed ones after reading their error. The user can see children in the Sub-agents workspace."
    )
});
static SERVER_INSTRUCTIONS_MAILBOXES_DISABLED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{DELEGATION_ROUTING}{INPUT_ROUTING} Finished children are parked and hold no processes; send_input resumes one with its conversation intact. Close children you no longer need, including failed ones after reading their error. The user can see children in the Sub-agents workspace."
    )
});

/// Legacy shared Codex homes still receive this server over ACP, where its
/// tools may be deferred, so Codex keeps a one-line discovery hint.
static CODEX_SERVER_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} If these tools are not visible, find mj-agents in the tool catalog; code mode exposes it as ALL_TOOLS. In code mode a wait runs inside an exec script that yields to you while the wait is still blocking; when that happens, poll that script with the longest yield your exec tool allows, never one second, and do other work between polls only when you have some: every poll re-sends your whole context, and in one measured run second-by-second polls were a quarter of the parent's cost.",
        SERVER_INSTRUCTIONS.as_str()
    )
});
static CODEX_SERVER_INSTRUCTIONS_MAILBOXES_DISABLED: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} If these tools are not visible, find mj-agents in the tool catalog; code mode exposes it as ALL_TOOLS. In code mode a wait runs inside an exec script that yields to you while the wait is still blocking; when that happens, poll that script with the longest yield your exec tool allows, never one second, and do other work between polls only when you have some: every poll re-sends your whole context, and in one measured run second-by-second polls were a quarter of the parent's cost.",
        SERVER_INSTRUCTIONS_MAILBOXES_DISABLED.as_str()
    )
});

/// A child's server instructions: its report reaches the parent through `handback`.
static CHILD_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "You are a Mjolnir sub-agent working for another session. Deliver your report for each task by calling handback once, as your last action. The session that started you reads that report, not the rest of this conversation. If a parent decision blocks further progress, hand back your question and stop; its answer arrives as your next prompt. Your report directory is named in your first prompt. {}",
        mj_core::subagent::HANDBACK_REPORT_RULES
    )
});

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

/// What the worker can say about a request the daemon has not answered. The
/// worker sends it with an answer that has no result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DaemonContact {
    /// Whether the daemon has read the worker's queue since this request
    /// joined it.
    pub picked_up: bool,
    /// Seconds since the daemon last read the queue; absent when it has not
    /// read it since the worker started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_collected_seconds_ago: Option<u64>,
}

impl DaemonContact {
    /// Read it from a worker answer. An older worker does not send it.
    fn from_reply(reply: &Value) -> Option<Self> {
        serde_json::from_value(reply.get("daemon")?.clone()).ok()
    }

    /// One sentence naming a daemon that has not picked the request up.
    fn not_picked_up(self) -> Option<String> {
        if self.picked_up {
            return None;
        }
        let last = match self.last_collected_seconds_ago {
            Some(seconds) => format!(
                "Its daemon last collected this session's requests {seconds} seconds ago"
            ),
            None => {
                "Its daemon has not collected this session's requests since this session's worker started".to_owned()
            }
        };
        Some(format!(
            "Mjolnir has not picked up this request. {last}, so the daemon is probably restarting, stalled, or unreachable."
        ))
    }
}

/// What to do about a request the daemon has not picked up. It stays saved
/// and runs once a daemon reads the queue, which can be long after this call.
fn not_picked_up_advice(action: &SubagentToolAction) -> &'static str {
    match action {
        SubagentToolAction::Handback { .. } => HANDBACK_UNANSWERED_ADVICE,
        SubagentToolAction::ListAgents | SubagentToolAction::ListProfiles => {
            "It changes nothing, so call it again later."
        }
        SubagentToolAction::Spawn { .. } => {
            "The request is saved and runs when the daemon picks it up, which can be much later, so do not repeat it. Continue without this child or call list_agents later to see whether it started; close it if you no longer need it."
        }
        _ => {
            "The request is saved and runs when the daemon picks it up, which can be much later, so do not repeat it. Call list_agents later to see whether it ran."
        }
    }
}

/// The degraded answer when the worker has taken the request but the daemon
/// has not completed it in time. The request stays queued; the model must
/// check on it itself, because nothing is ever pushed into its conversation.
/// When the worker says the daemon has not even picked the request up, the
/// answer says so and is an error: nothing is working on it.
fn pending_reply(
    request_id: &str,
    action: &SubagentToolAction,
    daemon: Option<DaemonContact>,
) -> (Value, bool) {
    if let Some(reason) = daemon.and_then(DaemonContact::not_picked_up) {
        return (
            json!({
                "request_id": request_id,
                "accepted": true,
                "status": "not_picked_up",
                "error": format!("{reason} {}", not_picked_up_advice(action)),
            }),
            true,
        );
    }
    let status = if daemon.is_some() {
        "Mjolnir picked up this request but has not finished it."
    } else {
        "Mjolnir has not answered this request yet."
    };
    (
        json!({
            "request_id": request_id,
            "accepted": true,
            "note": format!("{status} {}", unanswered_advice(action)),
        }),
        false,
    )
}

/// How long any action other than `wait` may take to be answered. The daemon
/// must notice the queued request, run the action (a spawn may provision a
/// child session) and complete it back to the worker. The worker answers a
/// little sooner by itself, so it can say whether the daemon picked it up.
pub(crate) const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

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
        "next_action":"Call wait to observe all of your children. This child remains in the wait set while its state is \"stopping\" and leaves it once teardown reaches \"stopped\". Do not spawn a replacement child before then."
    })
}

/// Slack added to the harness's `wait` window. The worker answers a `wait` at
/// the deadline itself, so this covers only the hop back from the worker.
const WAIT_REPLY_GRACE: Duration = Duration::from_secs(30);

/// How long the shim waits for the worker to answer `action`. A `wait` uses
/// the harness's default window plus grace; everything else is bounded by
/// [`REPLY_TIMEOUT`].
fn reply_timeout(action: &SubagentToolAction, harness: Option<HarnessKind>) -> Duration {
    match action {
        SubagentToolAction::WaitAgents => {
            mj_core::subagent::subagent_wait_timeout_for(harness) + WAIT_REPLY_GRACE
        }
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
    if let SubagentToolAction::WaitAgents = action {
        let mut payload = mj_core::subagent::still_running_payload(
            waited.as_secs(),
            Some(
                "Mjolnir did not answer this wait in time, so the current child states are unknown. \
                 Call wait again later to collect any reports that are ready.",
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
    let SubagentToolAction::WaitAgents = action else {
        return None;
    };
    Some(format!(
        "waiting for a new report from any child that is still running; {}s elapsed",
        elapsed.as_secs()
    ))
}

pub fn run_mcp_stdio(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    agent_mailboxes_enabled: bool,
) -> Result<()> {
    let stdin = std::io::stdin();
    run_with_mailboxes(
        stdin.lock(),
        std::io::stdout(),
        socket,
        harness,
        role,
        agent_mailboxes_enabled,
    )
}

/// Serve MCP over `reader`/`writer` against the worker `socket`. Calls are
/// dispatched concurrently, each on its own socket connection, so a long
/// `wait` never blocks a cheap `list_agents` queued after it. `harness` is the
/// parent's own harness, whose client decides how long one call may stay open.
/// `role` picks the tool set: a parent delegates, a child hands back.
#[cfg(test)]
fn run<R: BufRead, W: Write + Send + Sync + 'static>(
    reader: R,
    writer: W,
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
) -> Result<()> {
    run_with_mailboxes(reader, writer, socket, harness, role, true)
}

fn run_with_mailboxes<R: BufRead, W: Write + Send + Sync + 'static>(
    reader: R,
    writer: W,
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    agent_mailboxes_enabled: bool,
) -> Result<()> {
    let socket = socket.to_path_buf();
    let parent_instructions = match (harness, agent_mailboxes_enabled) {
        (Some(HarnessKind::Codex), true) => CODEX_SERVER_INSTRUCTIONS.as_str(),
        (Some(HarnessKind::Codex), false) => CODEX_SERVER_INSTRUCTIONS_MAILBOXES_DISABLED.as_str(),
        (_, true) => SERVER_INSTRUCTIONS.as_str(),
        (_, false) => SERVER_INSTRUCTIONS_MAILBOXES_DISABLED.as_str(),
    };
    let (instructions, tools) = match role {
        SubagentMcpRole::Parent => (
            parent_instructions,
            tool_definitions_with_mailboxes(harness, agent_mailboxes_enabled),
        ),
        SubagentMcpRole::FixedParent => (
            parent_instructions,
            fixed_tool_definitions_with_mailboxes(harness, agent_mailboxes_enabled),
        ),
        SubagentMcpRole::Child => (CHILD_INSTRUCTIONS.as_str(), child_tool_definitions()),
    };
    crate::mcp_stdio::serve(
        reader,
        writer,
        crate::mcp_stdio::McpServer {
            name: mj_core::subagent::SUBAGENT_MCP_SERVER,
            instructions,
            tools,
            progress_interval: crate::mcp_stdio::PROGRESS_INTERVAL,
            call: move |params: Option<&Value>, progress: &crate::mcp_stdio::Progress| {
                call_with_mailboxes(
                    &socket,
                    harness,
                    role,
                    params,
                    progress,
                    agent_mailboxes_enabled,
                )
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

/// Reject obsolete arguments rather than silently discarding assignment text.
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
#[serde(deny_unknown_fields)]
struct WaitArgs {}

#[cfg(test)]
fn call(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
) -> Result<(Value, bool)> {
    call_with_mailboxes(socket, harness, role, params, progress, true)
}

fn call_with_mailboxes(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
    agent_mailboxes_enabled: bool,
) -> Result<(Value, bool)> {
    call_with_budget_and_mailboxes(
        socket,
        harness,
        role,
        params,
        progress,
        |action| reply_timeout(action, harness),
        agent_mailboxes_enabled,
    )
}

/// Answer one tool call, waiting for the worker as long as `budget` allows
/// for the call's action.
#[cfg(test)]
fn call_with_budget(
    socket: &Path,
    _harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
    budget: impl Fn(&SubagentToolAction) -> Duration,
) -> Result<(Value, bool)> {
    call_with_budget_and_mailboxes(socket, _harness, role, params, progress, budget, true)
}

fn call_with_budget_and_mailboxes(
    socket: &Path,
    _harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
    budget: impl Fn(&SubagentToolAction) -> Duration,
    agent_mailboxes_enabled: bool,
) -> Result<(Value, bool)> {
    let params: CallParams = serde_json::from_value(params.cloned().context("missing params")?)?;
    if !role.tool_names().contains(&params.name.as_str()) {
        bail!("unknown sub-agent tool {:?} for {role}", params.name);
    }
    if role == SubagentMcpRole::FixedParent
        && params.name == "spawn"
        && ["profile_id", "model", "effort"]
            .iter()
            .any(|key| params.arguments.get(key).is_some())
    {
        bail!(
            "single-model spawn does not accept profile_id, model, or effort; the user fixed them for this session"
        );
    }
    let action = match params.name.as_str() {
        "list_profiles" => SubagentToolAction::ListProfiles,
        "spawn" => {
            if ["files", "context"]
                .iter()
                .any(|key| params.arguments.get(key).is_some())
            {
                bail!(
                    "spawn no longer accepts files or context; put context and file, symbol, line-range or earlier-report pointers in instructions"
                );
            }
            let args: SpawnArgs = serde_json::from_value(params.arguments)?;
            let instructions = args.instructions.trim();
            if instructions.is_empty() {
                bail!("spawn instructions cannot be empty");
            }
            SubagentToolAction::Spawn {
                task_name: args.task_name,
                instructions: instructions.to_owned(),
                profile_id: args.profile_id,
                model: args.model,
                effort: args.effort,
                working_directory: args.working_directory,
                // Legacy accepted requests keep these wire fields; new calls omit them.
                context: None,
                files: Vec::new(),
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
        "send_message" => {
            if !agent_mailboxes_enabled {
                bail!("send_message is unavailable while agent mailboxes are disabled");
            }
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            let message = args.message.context("send_message requires message")?;
            SubagentToolAction::SendMessage {
                child_session_id: args.child_session_id,
                message,
            }
        }
        "wait" => {
            let _args: WaitArgs = serde_json::from_value(params.arguments)?;
            SubagentToolAction::WaitAgents
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
        originating_command_id: None,
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
    let daemon = DaemonContact::from_reply(&reply);
    if let SubagentToolAction::CloseAgent { child_session_id } = &request.action {
        let reason = match daemon.and_then(DaemonContact::not_picked_up) {
            Some(reason) => format!(
                "{reason} The close is saved and runs when the daemon picks it up, so this child is not known to be gone."
            ),
            None => "Mjolnir took this close but has not confirmed it, so this child is not known to be gone.".to_owned(),
        };
        return Ok((
            still_closing_reply(&request_id, child_session_id, &reason),
            false,
        ));
    }
    Ok(pending_reply(&request_id, &request.action, daemon))
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
        SubagentToolAction::WaitAgents => Some(timeout.saturating_sub(WAIT_REPLY_GRACE).as_secs()),
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

#[cfg(test)]
fn tool_definitions(_harness: Option<HarnessKind>) -> Vec<Value> {
    tool_definitions_with_mailboxes(_harness, true)
}

fn tool_definitions_with_mailboxes(
    _harness: Option<HarnessKind>,
    agent_mailboxes_enabled: bool,
) -> Vec<Value> {
    let child = json!({"type":"object","properties":{"child_session_id":{"type":"string"}},"required":["child_session_id"],"additionalProperties":false});
    let current = mj_core::subagent::CURRENT_MODEL;
    let model = format!(
        "A model value from list_profiles, or \"{current}\" for this session's own model. Unless profile_id is given, Mjolnir runs the child on the eligible profile that offers this model and has the most quota left (the lower of its 5-hour and weekly remaining)."
    );
    let mut definitions = vec![
        tool(
            "list_profiles",
            "List eligible sub-agent profiles and the models and efforts each offers. Profiles that offer the same models are listed once, as the one with the most quota left.",
            json!({"type":"object","additionalProperties":false}),
        ),
        tool(
            "spawn",
            "Start a child in your target and filesystem. Returns child_session_id and report_dir immediately; registration does not mean startup succeeded. Collect the report or startup error with wait or list_agents. Reports are short and point to files in report_dir. Supply the assignment and starting pointers in instructions. Use send_input for follow-up work when an idle child's context helps. Choose a model from list_profiles; Mjolnir selects an eligible profile with the most quota unless profile_id pins one. An unavailable model, effort or login is an error, never replaced by another selection. A login the provider has refused stays unavailable until repaired with `mj login`. Only children holding processes count against the live children limit; Mjolnir parks it when a child hands back. A refused spawn names the live children. Close children you no longer need; failed-start cleanup must finish before a replacement can use its slot. A child in error cannot be re-prompted.",
            json!({
                "type":"object",
                "properties":{
                    "task_name":{"type":"string"},"instructions":{"type":"string","description":"The assignment, context, constraints and required evidence. Point to relevant files, symbols, line ranges and earlier reports; the child reads them in the shared filesystem."},
                    "profile_id":{"type":"string","description":"Only to pin one profile; normally omit it and let the model choose the profile."},"model":{"type":"string","description":model},"effort":{"type":"string","description":"An effort the chosen profile offers. Omitted, the child uses this session's effort when the chosen profile offers it, otherwise the harness default."},
                    "working_directory":{"type":"string","description":"Launch directory for the child session on the parent's target. Absolute paths are used as-is; relative paths resolve against the parent session's working directory. The directory must exist; no other restriction applies. Defaults to the parent session's working directory."}
                },
                "required":["task_name","instructions","model"],"additionalProperties":false
            }),
        ),
        tool(
            "list_agents",
            "List this parent's Mjolnir child sessions and status, including pending_inputs and recent input_deliveries, plus pending_messages and recent message_deliveries.",
            json!({"type":"object","additionalProperties":false}),
        ),
        tool(
            "send_input",
            "Queue follow-up input for one child session. Returns status queued after durable storage, not a turn_id or delivery confirmation. Input waits for startup and is delivered in order; collect delivery failures and delivery status with wait or list_agents, and the child's report with wait. Do not resend acknowledged input. A child whose turn ended is parked (wait and list_agents show parked true) and holds no processes: send_input queues input and starts it again in the background, with its conversation intact, which can take tens of seconds. If restarting a parked child exceeds the live-child limit or startup fails, wait/list_agents report the delivery failure; the child stays parked. Retry only after checking that result.",
            json!({"type":"object","properties":{"child_session_id":{"type":"string"},"message":{"type":"string"}},"required":["child_session_id","message"],"additionalProperties":false}),
        ),
        tool(
            "send_message",
            "Store a message for one child in its mailbox. A running child sees it at its next tool boundary without its turn being cancelled; an idle child wakes, and a parked child is restarted before delivery. This does not start a new turn. Check pending_messages and message_deliveries in wait or list_agents before retrying.",
            json!({"type":"object","properties":{"child_session_id":{"type":"string"},"message":{"type":"string"}},"required":["child_session_id","message"],"additionalProperties":false}),
        ),
        tool(
            "wait",
            &format!(
                "Wait for every child of yours that is not stopped. A child that you closed is not covered for report collection, even if it finished just before close. It returns at once when any child has a new report, when no child is unfinished, or when this harness's wait window ends. A report is returned only once for each finish; a child resumed with send_input can report again after its next turn. Status reported means one or more new reports are in output; nothing_to_wait_for means no new report or unfinished child; still_running means the wait window ended first. Entries without a new report have no output. A wait may end before work is done, and another wait is normal. A finished child has parked true when its processes were released; send_input resumes it. A child being closed may remain as a status-only entry while state is \"stopping\" and leaves the wait result when stopped. Pending input keeps a child unfinished: pending_inputs names queued requests and input_deliveries records delivery outcomes. Mailbox messages appear in pending_messages until accepted by the child's worker, then in message_deliveries. Delivery failures report state failed and their cause. A reminder to hand back keeps a child running. A refused login reports failure kind login_invalid and profile_id; this is not about the task. Its output names `mj login`. Details are in report_dir; output over {max_output} characters is truncated.",
                max_output = mj_core::subagent::MAX_HANDBACK_CHARS
            ),
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        tool(
            "close",
            "Stop one child session and retain its conversation. Stopping a running child is not instant: it checkpoints the child, seals its transcript and tears its process tree down; a parked child has no processes left and only its record is settled. An answer of closed true means that finished; any other answer, including status still_closing, means the child may still be on its way out. When you are replacing a child, call wait after close and wait until the closing child is no longer listed before spawning its replacement; a child that is still stopping holds its share of this target's processes, and starting the next one on top of it can exhaust them.",
            child,
        ),
    ];
    if !agent_mailboxes_enabled {
        definitions.retain(|tool| tool["name"] != "send_message");
    }
    definitions
}

#[cfg(test)]
fn fixed_tool_definitions(harness: Option<HarnessKind>) -> Vec<Value> {
    fixed_tool_definitions_with_mailboxes(harness, true)
}

fn fixed_tool_definitions_with_mailboxes(
    harness: Option<HarnessKind>,
    agent_mailboxes_enabled: bool,
) -> Vec<Value> {
    let mut tools = tool_definitions_with_mailboxes(harness, agent_mailboxes_enabled);
    tools.retain(|tool| tool["name"] != "list_profiles");
    let spawn = tools
        .iter_mut()
        .find(|tool| tool["name"] == "spawn")
        .expect("spawn tool");
    for key in ["profile_id", "model", "effort"] {
        spawn["inputSchema"]["properties"]
            .as_object_mut()
            .expect("properties")
            .remove(key);
    }
    spawn["inputSchema"]["required"] = json!(["task_name", "instructions"]);
    spawn["description"] = json!(
        "Start a child in your target and filesystem using the model and effort fixed by the user. Returns child_session_id and report_dir immediately; registration does not mean startup succeeded. Collect reports or startup errors with wait or list_agents. Reports are short and point to files in report_dir. Supply the assignment and file, symbol, line-range or earlier-report pointers in instructions. Prefer send_input for follow-up work when an idle child's context helps. Mjolnir selects an eligible profile with the most quota supporting the exact model and effort. An unavailable model, effort or login is an error, never replaced by another selection. Only children holding processes count against the live children limit; finished children are parked. Close children you no longer need, including failed children after reading the error. Failed-start cleanup must finish before a replacement can use its slot. A child in error cannot be re-prompted."
    );
    tools
}

/// A child's only tool: its report to the session that started it.
fn child_tool_definitions() -> Vec<Value> {
    vec![tool(
        "handback",
        &format!(
            "Deliver the task report to your parent, once as your last action. Repeating it in the same turn is refused. Your parent reads this report, not your conversation. {}",
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
    use std::sync::{Arc, Mutex};

    struct ContractWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for ContractWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn initialize_and_tool_list_deliver_concise_parent_and_child_contracts() {
        for role in [
            SubagentMcpRole::Parent,
            SubagentMcpRole::FixedParent,
            SubagentMcpRole::Child,
        ] {
            for harness in [HarnessKind::Claude, HarnessKind::Codex] {
                let input = format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
                );
                let output = Arc::new(Mutex::new(Vec::new()));
                run(
                    input.as_bytes(),
                    ContractWriter(output.clone()),
                    Path::new("unused.sock"),
                    Some(harness),
                    role,
                )
                .unwrap();
                let output = output.lock().unwrap();
                let replies: Vec<Value> = std::str::from_utf8(&output)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                let instructions =
                    replies.iter().find(|r| r["id"] == 1).unwrap()["result"]["instructions"]
                        .as_str()
                        .unwrap();
                // Claude Code keeps only the first 2,048 characters of a
                // server's instructions and drops the rest unseen.
                if harness != HarnessKind::Codex {
                    assert!(
                        instructions.chars().count() <= 2048,
                        "{role} {harness:?} instructions are {} characters",
                        instructions.chars().count()
                    );
                }
                let tools = replies.iter().find(|r| r["id"] == 2).unwrap()["result"]["tools"]
                    .as_array()
                    .unwrap();
                for tool in tools {
                    let description = tool["description"].as_str().unwrap();
                    assert!(
                        description.len() <= 1800,
                        "{role} {harness:?} {} description is {} bytes",
                        tool["name"],
                        description.len()
                    );
                    assert!(!description.contains(DELEGATION_ROUTING));
                }
                if role == SubagentMcpRole::Child {
                    assert!(!instructions.contains(DELEGATION_ROUTING));
                    assert!(instructions.contains(mj_core::subagent::HANDBACK_REPORT_RULES));
                } else {
                    assert!(instructions.contains(DELEGATION_ROUTING));
                    assert!(
                        instructions
                            .contains("Before spawning, give follow-up work through send_input")
                    );
                    assert_eq!(
                        instructions.contains("ALL_TOOLS"),
                        harness == HarnessKind::Codex
                    );
                    let spawn = tools.iter().find(|t| t["name"] == "spawn").unwrap();
                    let props = &spawn["inputSchema"]["properties"];
                    assert!(props.get("files").is_none() && props.get("context").is_none());
                    assert_eq!(
                        props.get("model").is_some(),
                        role == SubagentMcpRole::Parent
                    );
                    assert_eq!(spawn["inputSchema"]["additionalProperties"], false);
                    assert!(tools.iter().any(|t| t["name"] == "send_message"));
                    assert!(!tools.iter().any(|t| t["name"] == "interrupt"));
                    let send_message = tools.iter().find(|t| t["name"] == "send_message").unwrap();
                    assert_eq!(
                        send_message["inputSchema"]["required"],
                        json!(["child_session_id", "message"])
                    );
                    assert_eq!(send_message["inputSchema"]["additionalProperties"], false);
                    assert!(
                        send_message["description"]
                            .as_str()
                            .unwrap()
                            .contains("without its turn being cancelled")
                    );
                }
            }
        }
    }

    #[test]
    fn fixed_parent_exposes_no_selector_arguments_and_refuses_hidden_tools_and_overrides() {
        let tools = fixed_tool_definitions(None);
        assert!(!tools.iter().any(|tool| tool["name"] == "list_profiles"));
        assert!(tools.iter().any(|tool| tool["name"] == "send_message"));
        assert!(!tools.iter().any(|tool| tool["name"] == "interrupt"));
        let spawn = tools.iter().find(|tool| tool["name"] == "spawn").unwrap();
        assert_eq!(
            spawn["inputSchema"]["required"],
            json!(["task_name", "instructions"])
        );
        let mut calls = vec![json!({"name":"list_profiles", "arguments":{}})];
        for key in ["profile_id", "model", "effort"] {
            assert!(spawn["inputSchema"]["properties"].get(key).is_none());
            let mut args = json!({"task_name":"probe", "instructions":"report"});
            args[key] = json!("override");
            calls.push(json!({"name":"spawn", "arguments":args}));
        }
        for params in calls {
            let error = call(
                Path::new("unused.sock"),
                None,
                SubagentMcpRole::FixedParent,
                Some(&params),
                &crate::mcp_stdio::Progress::silent(Duration::from_secs(1)),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unknown sub-agent tool")
                    || error.to_string().contains("does not accept"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn disabled_mailboxes_omit_send_message_from_the_mcp_server() {
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        run_with_mailboxes(
            input.as_bytes(),
            ContractWriter(output.clone()),
            Path::new("unused.sock"),
            Some(HarnessKind::Claude),
            SubagentMcpRole::Parent,
            false,
        )
        .unwrap();
        let output = output.lock().unwrap();
        let replies = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let instructions =
            replies.iter().find(|reply| reply["id"] == 1).unwrap()["result"]["instructions"]
                .as_str()
                .unwrap();
        assert!(!instructions.contains("send_message"));
        let tools = replies.iter().find(|reply| reply["id"] == 2).unwrap()["result"]["tools"]
            .as_array()
            .unwrap();
        assert!(!tools.iter().any(|tool| tool["name"] == "send_message"));
        assert!(!tools.iter().any(|tool| tool["name"] == "interrupt"));

        let error = call_with_budget_and_mailboxes(
            Path::new("unused.sock"),
            None,
            SubagentMcpRole::Parent,
            Some(&json!({
                "name":"send_message",
                "arguments":{"child_session_id":"child","message":"note"}
            })),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_millis(50),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("mailboxes are disabled"));
    }

    /// #1160: a parent read a child's failed login as the child's report and
    /// gave up on the profile. The tools say what a refused login looks like
    /// and what to do about it.
    // Hard-won: #1160: a refused provider login was mistaken for a child report.
    #[test]
    fn wait_and_spawn_say_what_a_refused_login_looks_like() {
        let description = |name: &str| {
            tool_definitions(None)
                .into_iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["description"].as_str().map(str::to_owned))
                .unwrap()
        };
        let wait = description("wait");
        assert!(wait.contains("failure kind login_invalid"), "{wait}");
        assert!(wait.contains("`mj login`"), "{wait}");
        assert!(wait.contains("not about the task"), "{wait}");
        let spawn = description("spawn");
        assert!(spawn.contains("login the provider has refused"), "{spawn}");
    }

    /// #1161: a child that handed back is parked. The parent has to know
    /// that such a child holds no processes, that `send_input` starts it
    /// again and can take a while, and that the cap counts live children.
    // Hard-won: #1161: idle sub-agent process trees exhausted the container.
    #[test]
    fn the_tools_explain_parked_children_and_the_live_child_limit() {
        let description = |name: &str| {
            tool_definitions(None)
                .into_iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["description"].as_str().map(str::to_owned))
                .unwrap()
        };
        let send_input = description("send_input");
        assert!(send_input.contains("parked true"), "{send_input}");
        assert!(send_input.contains("starts it again"), "{send_input}");
        assert!(send_input.contains("tens of seconds"), "{send_input}");
        let wait = description("wait");
        assert!(wait.contains("parked true"), "{wait}");
        let spawn = description("spawn");
        assert!(spawn.contains("live children"), "{spawn}");
        assert!(spawn.contains("parks it"), "{spawn}");
        assert!(SERVER_INSTRUCTIONS.contains("Finished children are parked"));
        assert!(SERVER_INSTRUCTIONS.contains("send_input resumes one"));
        // Codex code mode runs a wait inside an exec script that yields while
        // the wait blocks; the S30 parents then polled that script every second.
        assert!(CODEX_SERVER_INSTRUCTIONS.contains("longest yield"));
        assert!(!SERVER_INSTRUCTIONS.contains("longest yield"));
    }

    #[test]
    fn wait_advertises_no_arguments() {
        let wait = tool_definitions(None)
            .into_iter()
            .find(|tool| tool["name"] == "wait")
            .expect("wait definition");
        assert_eq!(wait["inputSchema"]["properties"], json!({}));
        assert_eq!(wait["inputSchema"]["additionalProperties"], false);
    }

    // Hard-won: fcbd178e: the server promised pushed results that only wait delivered.
    #[test]
    fn instructions_collect_results_through_wait_and_never_promise_a_push() {
        assert!(SERVER_INSTRUCTIONS.contains("wait"));
        assert!(
            !SERVER_INSTRUCTIONS.contains("arrive in"),
            "instructions must not promise pushed results: {SERVER_INSTRUCTIONS:?}"
        );
        // Each wait call resends the parent's whole context, so the parent is
        // told to read details from files.
        for needed in ["report_dir", "instead of duplicating work"] {
            assert!(
                SERVER_INSTRUCTIONS.contains(needed),
                "{needed}: {SERVER_INSTRUCTIONS:?}"
            );
        }
        let wait = tool_definitions(None)
            .into_iter()
            .find(|tool| tool["name"] == "wait")
            .expect("wait definition");
        let description = wait["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("new report") && description.contains("reported"),
            "{description}"
        );
        assert!(
            wait["inputSchema"]["properties"]
                .get("child_session_ids")
                .is_none()
        );
        assert!(
            wait["inputSchema"]["properties"]
                .get("return_when")
                .is_none()
        );
        assert!(
            wait["inputSchema"]["properties"]
                .get("timeout_seconds")
                .is_none()
        );
    }

    // Hard-won: cc7e6a2a: short repeated waits wasted parent context despite the harness ceiling.
    #[test]
    fn wait_description_explains_that_another_wait_is_normal() {
        let wait = tool_definitions(None)
            .into_iter()
            .find(|tool| tool["name"] == "wait")
            .expect("wait definition");
        let description = wait["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("A wait may end before work is done, and another wait is normal."),
            "{description}"
        );
        assert!(!description.contains("timeout_seconds"), "{description}");
        assert!(
            !description.contains("poll with short timeouts"),
            "{description}"
        );
    }

    // Hard-won: cc7e6a2a: oversized handbacks overwhelmed parent context.
    #[test]
    fn the_child_is_told_the_same_report_rules_everywhere() {
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // Exercise the instructions the harness actually receives, not just
        // the constants and tool constructors used to build them.
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        run(
            input.as_bytes(),
            SharedWriter(Arc::clone(&output)),
            Path::new("unused.sock"),
            None,
            SubagentMcpRole::Child,
        )
        .unwrap();
        let output = output.lock().unwrap();
        let responses: Vec<Value> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let initialized = responses.iter().find(|reply| reply["id"] == 1).unwrap();
        let listed = responses.iter().find(|reply| reply["id"] == 2).unwrap();
        let handback = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "handback")
            .unwrap();
        let prompt = mj_core::subagent::handback_prompt_note("/workspace/reports/child");
        for instructions in [
            prompt.as_str(),
            initialized["result"]["instructions"].as_str().unwrap(),
            handback["description"].as_str().unwrap(),
        ] {
            assert!(
                instructions.contains(mj_core::subagent::HANDBACK_REPORT_RULES),
                "{instructions}"
            );
        }
    }

    /// Answer one spawn the way a worker does when the daemon has not
    /// finished it: accepted, no result, and what the worker knows about the
    /// daemon.
    #[cfg(unix)]
    fn spawn_answered_by_worker(daemon: Value) -> (Value, bool) {
        use std::io::{BufReader, Write};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut stream = reader.into_inner();
            let mut body =
                serde_json::to_vec(&json!({"accepted": true, "daemon": daemon})).unwrap();
            body.push(b'\n');
            stream.write_all(&body).unwrap();
        });
        let answer = call_with_budget(
            &socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name": "spawn", "arguments": {"task_name": "t", "instructions": "i"}})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_secs(30),
        )
        .unwrap();
        worker.join().unwrap();
        answer
    }

    /// #1197: a parent's spawn and list_agents went unanswered while the
    /// daemon was stalled, and the model read only "did not answer". When the
    /// worker reports that the daemon never picked the request up, the model
    /// is told that, that the spawn is saved and may still start, and not to
    /// repeat it.
    #[cfg(unix)]
    // Hard-won: #1197: a queued spawn ran after the parent had given up.
    #[test]
    fn a_spawn_the_daemon_never_picked_up_names_the_daemon_and_forbids_a_repeat() {
        let (value, is_error) = spawn_answered_by_worker(
            json!({"picked_up": false, "last_collected_seconds_ago": 140}),
        );
        assert!(is_error, "{value}");
        assert_eq!(value["status"], "not_picked_up", "{value}");
        let error = value["error"].as_str().expect("error text");
        assert!(
            error.contains("has not picked up this request")
                && error.contains("140 seconds ago")
                && error.contains("restarting, stalled, or unreachable")
                && error.contains("do not repeat it")
                && error.contains("list_agents"),
            "{error}"
        );

        let (value, _) = spawn_answered_by_worker(json!({"picked_up": false}));
        let error = value["error"].as_str().expect("error text");
        assert!(
            error.contains("since this session's worker started"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_spawn_the_daemon_picked_up_but_did_not_finish_is_still_pending() {
        let (value, is_error) =
            spawn_answered_by_worker(json!({"picked_up": true, "last_collected_seconds_ago": 2}));
        assert!(!is_error, "{value}");
        assert_eq!(value["accepted"], true);
        let note = value["note"].as_str().expect("note text");
        assert!(
            note.contains("picked up this request but has not finished it")
                && note.contains("list_agents"),
            "{note}"
        );
    }

    // Hard-won: #1037: a lost worker reply left MCP calls blocked forever.
    #[test]
    fn wait_calls_use_the_harness_default_plus_grace() {
        let wait = SubagentToolAction::WaitAgents;
        assert_eq!(
            reply_timeout(&wait, None),
            Duration::from_secs(mj_core::subagent::DEFAULT_WAIT_SECONDS) + WAIT_REPLY_GRACE
        );
        assert_eq!(
            reply_timeout(&wait, Some(HarnessKind::Codex)),
            Duration::from_secs(mj_core::subagent::MAX_CODEX_WAIT_SECONDS) + WAIT_REPLY_GRACE
        );
        assert_eq!(
            reply_timeout(&SubagentToolAction::ListAgents, Some(HarnessKind::Codex)),
            REPLY_TIMEOUT
        );
    }

    #[cfg(unix)]
    // Hard-won: #1037: the shim blocked forever after a lost worker reply.
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
                "arguments": {}
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
        assert_eq!(
            value["agents"],
            json!([]),
            "the worker does not know child state"
        );
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
            "message": "{\"status\":\"reported\",\"agents\":[]}"
        });
        let value = model_facing("r-1", &envelope);
        assert_eq!(value["status"], "reported");
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
    // Hard-won: 91244ea1: Claude sub-agent handback stalled on an unapproved MCP tool.
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
            format!("{child:#}").contains("unknown sub-agent tool"),
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
    fn send_message_sends_a_mailbox_action_instead_of_a_turn_cancel() {
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
            let reply = json!({"accepted":true,"result":{
                "request_id":request_id,"completed_at_ms":1,"is_error":false,
                "message":"{\"child_session_id\":\"child\",\"status\":\"queued\"}"
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
            SubagentMcpRole::Parent,
            Some(&json!({
                "name":"send_message",
                "arguments":{"child_session_id":"child","message":"keep going"}
            })),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_secs(5),
        )
        .unwrap();
        assert!(!is_error, "{value}");
        assert_eq!(value["status"], "queued");
        let request = received.recv().unwrap();
        assert_eq!(
            request["action"],
            json!({
                "action":"send_message",
                "params":{"child_session_id":"child","message":"keep going"}
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_wait_accepts_no_arguments_and_omits_retired_fields() {
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
                "message": "{\"status\":\"nothing_to_wait_for\",\"agents\":[]}"
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
            Some(&json!({"name": "wait", "arguments": {}})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_secs(5),
        )
        .unwrap();
        assert!(!is_error, "{value}");
        let request = received.recv().unwrap();
        assert_eq!(request["action"], json!({"action":"wait_agents"}));
    }

    #[test]
    fn the_wait_schema_is_empty_and_rejects_extra_properties() {
        let tools = tool_definitions(Some(HarnessKind::Codex));
        let wait = tools
            .iter()
            .find(|tool| tool["name"] == "wait")
            .expect("wait tool");
        let schema = &wait["inputSchema"];
        assert!(schema.get("required").is_none(), "{schema}");
        assert_eq!(schema["properties"], json!({}), "{schema}");
        assert_eq!(schema["additionalProperties"], false, "{schema}");
        assert!(
            schema["properties"].get("child_session_ids").is_none(),
            "{schema}"
        );
        assert!(
            schema["properties"].get("return_when").is_none(),
            "{schema}"
        );
        let description = wait["description"].as_str().unwrap();
        assert!(
            description.contains("every child of yours that is not stopped"),
            "{description}"
        );
        assert!(
            description.contains("closed is not covered"),
            "{description}"
        );
        assert!(
            description.contains("A wait may end before work is done, and another wait is normal."),
            "{description}"
        );
        assert!(!description.contains("timeout_seconds"), "{description}");
    }

    #[test]
    fn the_wait_arguments_reject_retired_selectors() {
        for field in ["child_session_ids", "return_when", "timeout_seconds"] {
            let arguments = json!({field: true});
            assert!(
                serde_json::from_value::<WaitArgs>(arguments).is_err(),
                "{field}"
            );
        }
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
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"wait","arguments":{}}}),
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
    // Hard-won: #1087: premature close completion let replacement process trees overlap.
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
    // Hard-won: #1087: close was reported finished while teardown was still running.
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

    /// The parent model has to know a closing child remains a status-only wait
    /// entry until teardown finishes, and a replacement must wait.
    // Hard-won: #1087: premature replacement exhausted container process slots.
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
        assert!(close.contains("no longer listed"), "{close}");
        assert!(
            close.contains("replacement"),
            "the close must say what to do before spawning the next child: {close}"
        );

        // And `wait` has to advertise that it follows a close at all, or the
        // advice above names a tool that says nothing about closing children.
        let wait = description("wait");
        assert!(wait.contains("closed"), "{wait}");
        assert!(
            wait.contains("\"stopping\"")
                && wait.contains("status-only entry")
                && wait.contains("leaves the wait result when stopped"),
            "{wait}"
        );
    }
}
