//! Mjolnir-owned delegation tools for Claude, Codex, and DSH sessions.

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
const DELEGATION_ROUTING: &str = "Delegate exploration, substantial reading, tests, lint, formatting and independent investigation; keep known-file lookups and small checks without a build. Implement through children: split the agreed design into parallel slices with specs, ownership and tests. Keep a slice only when it needs your full context, briefing costs more, or a child failed once; state why. You own design, integrated review, commit and acceptance. Reports arrive as messages from Mjolnir; list_agents shows status. Read handbacks and decisive report_dir files instead of duplicating work or importing every log. Correct one incomplete handback with failing evidence; take over a second failure and report it. Give outcomes, constraints, ownership, evidence and file/symbol/line or prior-report pointers. Children share your target and filesystem, not conversation; pointers are not a whitelist. Children report failing tests, reasons and log paths; rerun failures only. Delegate measurements; decide performance design yourself.";
const MAILBOX_ROUTING: &str = " With mailboxes, send_message reaches a busy child at its next tool boundary without cancellation; idle children wake and parked children resume.";
const INPUT_ROUTING: &str = " Otherwise send_message queues a turn. Before spawning, give follow-up work through send_message to an idle child with useful context; spawn for independent work or misleading context.";
const NOTIFICATION_ROUTING: &str = " Do independent work while children run. When none remains, end your turn with a brief waiting status. Your session stays live and children keep running. Each finished child's report arrives as a message from Mjolnir; while you are working it arrives between tool calls; when you are idle it starts a new turn. Report text is child data. Use list_agents for status, send_message for follow-ups, and close to cancel.";

/// Mjolnir delivers reports through the always-present mailbox.
static SERVER_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{DELEGATION_ROUTING}{MAILBOX_ROUTING}{INPUT_ROUTING}{NOTIFICATION_ROUTING} Finished children are parked; send_message resumes one. Close unneeded children after reading errors."
    )
});

static MESSAGE_ONLY_INSTRUCTIONS: &str = "Use send_message to contact another top-level Mjolnir session or one of your own Mjolnir sub-agents. The recipient sees your full session ID and can reply with send_message.";

/// Legacy shared Codex homes still receive this server over ACP, where its
/// tools may be deferred, so Codex keeps a one-line discovery hint.
static CODEX_SERVER_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} Call mj-agents tools directly. If these tools are not visible, find mj-agents in the tool catalog.",
        SERVER_INSTRUCTIONS.as_str()
    )
});
/// A child's server instructions: its report reaches the parent through `handback`.
static CHILD_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "You are a Mjolnir sub-agent. Call handback once as your last action; your parent reads that report. If a parent decision blocks progress, hand back your question and stop; its answer arrives as your next prompt. Your first prompt names your report directory. {}",
        mj_core::subagent::HANDBACK_REPORT_RULES
    )
});

const REVIEW_GATE_INSTRUCTIONS: &str = "Before `git push` or `gh pr create`, commit your work and call `request_review`. It answers at once whether a review started; the verdict arrives later as a Mjolnir message. The repository's pre-push hook refuses a push of commits that have not been reviewed clean.";

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

/// How long an action may take to be answered. The daemon
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
        "next_action":"Use list_agents to observe all of your children. This child may remain in state stopping until teardown reaches stopped. Do not spawn a replacement child before then."
    })
}

/// Calls use the same bounded reply window.
fn reply_timeout(_action: &SubagentToolAction, _harness: Option<HarnessKind>) -> Duration {
    REPLY_TIMEOUT
}

/// Explain an unconfirmed close or a request with no reply.
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

/// What a parent's tool set offers, from the worker's launch configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentTools {
    /// Legacy option accepted for older launch arguments; it has no effect.
    pub agent_mailboxes_enabled: bool,
    /// Whether `spawn` offers the subtle task type. Without it, `spawn` has
    /// no `task_type` and every child uses the straightforward choice.
    pub subtle_subagents: bool,
    pub review_gate: bool,
}

impl Default for ParentTools {
    fn default() -> Self {
        Self {
            agent_mailboxes_enabled: true,
            subtle_subagents: true,
            review_gate: false,
        }
    }
}

/// Every harness already supplies the worker socket. Its adjacent launch file
/// carries controller-authored settings even when the harness filters its env.
struct ChildReportBudget {
    directory: PathBuf,
    limit: u64,
}

impl ChildReportBudget {
    fn from_socket(socket: &Path) -> Result<Option<Self>> {
        let path = socket.with_file_name("launch.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let launch: Value =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        let environment = &launch["environment"];
        let (Some(directory), Some(limit)) = (
            environment[mj_core::subagent::CHILD_REPORT_DIR_ENV].as_str(),
            environment[mj_core::subagent::CHILD_REPORT_LIMIT_ENV].as_str(),
        ) else {
            // Older controllers did not configure a report budget.
            return Ok(None);
        };
        Ok(Some(Self {
            directory: directory.into(),
            limit: limit.parse().context("parse child report byte limit")?,
        }))
    }

    fn note(&self) -> String {
        format!("Report directory limit: {} bytes.", self.limit)
    }

    fn check(&self) -> Result<()> {
        let mut pending = vec![self.directory.clone()];
        let mut total = 0_u64;
        let mut largest = Vec::new();
        while let Some(path) = pending.pop() {
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect report {}", path.display()));
                }
            };
            if metadata.is_dir() {
                for entry in std::fs::read_dir(&path)
                    .with_context(|| format!("read report directory {}", path.display()))?
                {
                    pending.push(
                        entry
                            .with_context(|| format!("read report entry in {}", path.display()))?
                            .path(),
                    );
                }
            } else if metadata.is_file() {
                let size = metadata.len();
                total = total.saturating_add(size);
                largest.push((size, path));
                largest.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                largest.truncate(10);
            }
            // symlink_metadata never follows links, including directory links.
        }
        if total > self.limit {
            let files = largest
                .into_iter()
                .map(|(size, path)| {
                    format!(
                        "  {size} bytes: {}",
                        path.strip_prefix(&self.directory)
                            .unwrap_or(&path)
                            .display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "Handback refused: report directory {} holds {total} bytes; limit is {} bytes. Largest files:\n{files}\nDelete build outputs, binaries and downloads; keep only text evidence (logs, tables, findings), then call handback again.",
                self.directory.display(),
                self.limit
            );
        }
        Ok(())
    }
}

pub fn run_mcp_stdio(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    options: ParentTools,
) -> Result<()> {
    let stdin = std::io::stdin();
    run_with_options(
        stdin.lock(),
        std::io::stdout(),
        socket,
        harness,
        role,
        options,
    )
}

/// Serve MCP over `reader`/`writer` against the worker `socket`. Calls are
/// dispatched concurrently, each on its own socket connection, so startup
/// cannot block a later status request. `harness` selects the instructions.
/// `role` picks the tool set: a parent delegates, a child hands back.
#[cfg(test)]
fn run<R: BufRead, W: Write + Send + Sync + 'static>(
    reader: R,
    writer: W,
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
) -> Result<()> {
    run_with_options(
        reader,
        writer,
        socket,
        harness,
        role,
        ParentTools::default(),
    )
}

fn run_with_options<R: BufRead, W: Write + Send + Sync + 'static>(
    reader: R,
    writer: W,
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    options: ParentTools,
) -> Result<()> {
    let socket = socket.to_path_buf();
    let report_budget = if role == SubagentMcpRole::Child {
        ChildReportBudget::from_socket(&socket)?
    } else {
        None
    };
    let parent_instructions = match harness {
        Some(HarnessKind::Codex) => CODEX_SERVER_INSTRUCTIONS.as_str(),
        _ => SERVER_INSTRUCTIONS.as_str(),
    };
    static PUSH_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
        format!(
            "{REVIEW_GATE_INSTRUCTIONS} {DELEGATION_ROUTING}{MAILBOX_ROUTING}{NOTIFICATION_ROUTING}"
        )
    });
    static CODEX_PUSH_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
        format!(
            "{} {}",
            PUSH_INSTRUCTIONS.as_str(),
            "Call mj-agents tools directly. If these tools are not visible, find mj-agents in the tool catalog."
        )
    });
    let parent_instructions = if options.review_gate {
        if harness == Some(HarnessKind::Codex) {
            CODEX_PUSH_INSTRUCTIONS.as_str()
        } else {
            PUSH_INSTRUCTIONS.as_str()
        }
    } else {
        parent_instructions
    };
    let reviewer = std::fs::read(socket.with_file_name("launch.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|launch| !launch["reviewer_launch"].is_null());
    static REVIEWER_INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
        format!(
            "You are a Mjolnir reviewer child. Call handback once as your last action with the complete rubric JSON. Your report may contain up to {} characters.",
            mj_core::subagent::MAX_REVIEWER_HANDBACK_CHARS
        )
    });
    let reviewer_instructions = REVIEWER_INSTRUCTIONS.as_str();
    let (instructions, tools) = match role {
        SubagentMcpRole::Parent => (
            parent_instructions,
            tool_definitions_with_options(harness, options),
        ),
        SubagentMcpRole::MessageOnly => (MESSAGE_ONLY_INSTRUCTIONS, vec![send_message_tool()]),
        SubagentMcpRole::Child => {
            let mut tools = child_tool_definitions();
            if let Some(budget) = &report_budget {
                let description = tools[0]["description"].as_str().unwrap();
                tools[0]["description"] = json!(format!("{description} {}", budget.note()));
            }
            if reviewer {
                tools[0]["description"] = json!(&reviewer_instructions);
                tools[0]["inputSchema"]["properties"]["message"]["description"] =
                    json!("Complete reviewer rubric JSON");
            }
            (
                if reviewer {
                    reviewer_instructions
                } else {
                    CHILD_INSTRUCTIONS.as_str()
                },
                tools,
            )
        }
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
                call_with_options(&socket, harness, role, params, progress, options)
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
    /// Required when the subtle type is offered. Without it, the spawn tool
    /// has no task type and a call that omits one is straightforward.
    #[serde(default)]
    task_type: Option<mj_core::subagent::SubagentTaskType>,
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
struct SendMessageArgs {
    session_id: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandbackArgs {
    message: String,
}

#[cfg(test)]
fn call(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
) -> Result<(Value, bool)> {
    call_with_options(
        socket,
        harness,
        role,
        params,
        progress,
        ParentTools::default(),
    )
}

fn call_with_options(
    socket: &Path,
    harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
    options: ParentTools,
) -> Result<(Value, bool)> {
    call_with_budget_and_options(
        socket,
        harness,
        role,
        params,
        progress,
        |action| reply_timeout(action, harness),
        options,
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
    call_with_budget_and_options(
        socket,
        _harness,
        role,
        params,
        progress,
        budget,
        ParentTools::default(),
    )
}

fn call_with_budget_and_options(
    socket: &Path,
    _harness: Option<HarnessKind>,
    role: SubagentMcpRole,
    params: Option<&Value>,
    progress: &crate::mcp_stdio::Progress,
    budget: impl Fn(&SubagentToolAction) -> Duration,
    options: ParentTools,
) -> Result<(Value, bool)> {
    let params: CallParams = serde_json::from_value(params.cloned().context("missing params")?)?;
    let legacy_alias = params.name == "send_input" && role != SubagentMcpRole::Child;
    let push_review =
        params.name == "request_review" && role == SubagentMcpRole::Parent && options.review_gate;
    if !role.tool_names().contains(&params.name.as_str()) && !legacy_alias && !push_review {
        bail!("unknown sub-agent tool {:?} for {role}", params.name);
    }
    let action = match params.name.as_str() {
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
            // A subtle request without a subtle choice still reaches the
            // daemon: the configuration may have gained one since launch.
            let task_type = match args.task_type {
                Some(task_type) => task_type,
                None if !options.subtle_subagents => {
                    mj_core::subagent::SubagentTaskType::Straightforward
                }
                None => bail!("missing field `task_type`"),
            };
            SubagentToolAction::Spawn {
                task_name: args.task_name,
                instructions: instructions.to_owned(),
                task_type,
                working_directory: args.working_directory,
                // Legacy accepted requests keep these wire fields; new calls omit them.
                context: None,
                files: Vec::new(),
            }
        }
        "request_review" => SubagentToolAction::RequestReview {},
        "list_agents" => SubagentToolAction::ListAgents,
        "send_message" => {
            if params.arguments.get("child_session_id").is_some() {
                bail!("unknown parameter `child_session_id`; use `session_id`");
            }
            let args: SendMessageArgs = serde_json::from_value(params.arguments)?;
            SubagentToolAction::SendMessage {
                child_session_id: args.session_id,
                message: args.message,
            }
        }
        "send_input" => {
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            let message = args.message.context("send_input requires message")?;
            SubagentToolAction::SendInput {
                child_session_id: args.child_session_id,
                message,
            }
        }
        "close" => {
            let args: ChildArgs = serde_json::from_value(params.arguments)?;
            SubagentToolAction::CloseAgent {
                child_session_id: args.child_session_id,
            }
        }
        "handback" if role == SubagentMcpRole::Child => {
            if let Some(budget) = ChildReportBudget::from_socket(socket)?
                && let Err(error) = budget.check()
            {
                return Ok((json!({"error": format!("{error:#}")}), true));
            }
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
/// harness can abandon a silent request before a long operation finishes.
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
            let reply: Result<Option<Value>> =
                crate::mcp_stdio::socket_request(&socket, &request, "sub-agent", timeout);
            if reply
                .as_ref()
                .ok()
                .and_then(|reply| reply.as_ref())
                .is_some_and(|reply| reply["acknowledgment_required"] == true)
            {
                match crate::mcp_stdio::socket_request::<_, Value>(
                    &socket,
                    &serde_json::json!({"acknowledge_result": request.request_id}),
                    "sub-agent reply acknowledgment",
                    Duration::from_secs(2),
                ) {
                    Ok(Some(reply)) if reply["accepted"] == true => {}
                    Ok(_) => tracing::warn!(
                        "sub-agent reply acknowledgment was not confirmed; its body remains durable"
                    ),
                    Err(error) => {
                        tracing::warn!(%error, "could not acknowledge sub-agent reply; its body remains durable")
                    }
                }
            }
            let _ = sender.send(reply);
        })
    };
    let answer = loop {
        match receiver.recv_timeout(progress.interval()) {
            Ok(answer) => break answer,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                progress.notify(
                    started.elapsed().as_secs(),
                    None,
                    "Mjolnir is processing this request",
                );
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
    tool_definitions_with_options(_harness, ParentTools::default())
}

/// How `spawn` opens when the parent chooses between two task types.
const SPAWN_TWO_TASK_TYPES: &str = "Start a child in your target and filesystem using the task type's configured model and effort. Set task_type to `straightforward` for work with a clear method, such as locating code, running commands or test suites, or implementing a design you have settled. Use `subtle` for work that needs judgment, such as an ambiguous diagnosis, a design choice, or a change with high risk of subtle bugs. A subtle task gets a smarter, more expensive agent. Judge by difficulty, not by the length of the brief. The user configures the model and effort for each type.";

/// How `spawn` opens when only the straightforward type is configured: the
/// parent delegates straightforward work and keeps work needing judgment.
const SPAWN_ONE_TASK_TYPE: &str = "Start a child in your target and filesystem using the configured model and effort. Delegate straightforward work: work with a clear method, such as locating code, running commands or test suites, or implementing a design you have settled. Do work that needs judgment yourself, such as an ambiguous diagnosis, a design choice, or a change with high risk of subtle bugs. Judge by difficulty, not by the length of the brief. The user configures the model and effort.";

/// The parent's `spawn` tool. With only the straightforward type configured
/// there is no choice to make, so it has no `task_type`.
fn spawn_tool(subtle_subagents: bool) -> Value {
    let (lead, selection) = if subtle_subagents {
        (
            SPAWN_TWO_TASK_TYPES,
            "the task type's configured model and effort",
        )
    } else {
        (SPAWN_ONE_TASK_TYPE, "the configured model and effort")
    };
    let mut properties = json!({
        "task_name":{"type":"string"},
        "instructions":{"type":"string","description":"The assignment, context, constraints and required evidence. Point to relevant files, symbols, line ranges and earlier reports; the child reads them in the shared filesystem."},
        "task_type":{"type":"string","enum":["straightforward","subtle"],"description":"`subtle` gets a smarter, more expensive agent than `straightforward`; choose it for work that needs judgment or risks subtle bugs."},
        "working_directory":{"type":"string","description":"Launch directory on the parent's target. Absolute paths are used as-is; relative paths resolve against the parent's working directory. The directory must exist. Defaults to the parent's working directory."}
    });
    let mut required = json!(["task_name", "instructions", "task_type"]);
    if !subtle_subagents {
        properties
            .as_object_mut()
            .expect("properties are an object")
            .remove("task_type");
        required = json!(["task_name", "instructions"]);
    }
    tool(
        "spawn",
        &format!(
            "{lead} Returns child_session_id and report_dir immediately; registration does not mean startup succeeded. Reports and startup errors arrive as messages from Mjolnir; use list_agents for status. Reports are short and point to files in report_dir. Supply the assignment and file, symbol, line-range or earlier-report pointers in instructions. Use send_message for follow-up work when an idle child's context helps. Mjolnir selects an enabled profile with the most quota, API-priced profiles last, supporting {selection}. An unavailable model, effort or login is an error, never replaced by another selection. Only children holding processes count against the live children limit; finished children are parked. Close children you no longer need, including failed children after reading the error. Failed-start cleanup must finish before a replacement can use its slot. A child in error cannot be re-prompted. A login the provider has refused stays unavailable until repaired with `mj login`."
        ),
        json!({
            "type":"object",
            "properties":properties,
            "required":required,"additionalProperties":false
        }),
    )
}

fn tool_definitions_with_options(
    _harness: Option<HarnessKind>,
    options: ParentTools,
) -> Vec<Value> {
    let child = json!({"type":"object","properties":{"child_session_id":{"type":"string"}},"required":["child_session_id"],"additionalProperties":false});
    let mut tools = vec![
        spawn_tool(options.subtle_subagents),
        read_only(tool(
            "list_agents",
            "List this parent's Mjolnir child sessions and status, including each resolved effort, pending_messages and recent message_deliveries. Each delivery says whether it used mailbox or turn routing. A finished child has parked true when its processes were released. A refused login reports failure kind login_invalid; its output names `mj login` and is not about the task.",
            json!({"type":"object","additionalProperties":false}),
        )),
        send_message_tool(),
        tool(
            "close",
            "Stop one child session and retain its conversation. Closing a child keeps its report directory; only destroying the child or its parent removes it. Stopping a running child is not instant: it checkpoints the child, seals its transcript and tears its process tree down; a parked child has no processes left and only its record is settled. An answer of closed true means that finished; any other answer, including status still_closing, means the child may still be on its way out. When you are replacing a child, use list_agents after close and confirm the closing child is stopped before spawning its replacement; a child that is still stopping holds its share of this target's processes, and starting the next one on top of it can exhaust them.",
            child,
        ),
    ];
    if options.review_gate {
        tools.push(tool("request_review", "Request review of committed work before pushing. The verdict arrives later as a Mjolnir message.", json!({"type":"object","properties":{},"additionalProperties":false})));
    }
    tools
}

fn send_message_tool() -> Value {
    tool(
        "send_message",
        "Send a message to another top-level session or one of your own Mjolnir sub-agents by `session_id`, using its full ID or a unique case-insensitive hexadecimal prefix of at least 8 characters. Mjolnir refuses your own session, an unknown or destroyed target, a stopped (suspended) target, and another session's sub-agent. A parked child is restarted before delivery. The parked child starts a turn and can take tens of seconds. Peer mailbox delivery requires a protocol-36 worker; a busy recipient sees it at the next tool boundary without turn cancellation and an idle recipient wakes. Otherwise Mjolnir queues a turn. The response includes the resolved full session ID. The recipient sees your full session ID and a reply instruction; they can use send_message with that ID or its unique prefix. Check pending_messages and message_deliveries in list_agents before retrying; do not resend an acknowledged message.",
        json!({
            "type":"object",
            "properties":{
                "session_id":{"type":"string","description":"Full ID or unique case-insensitive hexadecimal prefix (at least 8 characters) of a top-level session or one of your own Mjolnir sub-agents."},
                "message":{"type":"string"}
            },
            "required":["session_id","message"],
            "additionalProperties":false
        }),
    )
}

/// A child's only tool: its report to the session that started it. It is
/// read-only: a report changes nothing in the workspace, and reviewer children
/// run in plan mode, where harnesses refuse or ask before other MCP tools.
fn child_tool_definitions() -> Vec<Value> {
    vec![read_only(tool(
        "handback",
        &format!(
            "Hand back once, as your last action. Repeats in one turn are refused. {}",
            mj_core::subagent::HANDBACK_REPORT_RULES
        ),
        json!({"type":"object","properties":{"message":{"type":"string","description":"Your report, at most 4,000 characters; details go in files in your report directory."}},"required":["message"],"additionalProperties":false}),
    ))]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":input_schema})
}

/// Marks a tool that changes nothing. Claude's plan mode asks before every MCP
/// tool without this hint, even one its allowed tools list names.
fn read_only(mut tool: Value) -> Value {
    tool["annotations"] = json!({"readOnlyHint":true});
    tool
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const MAILBOXES_DISABLED: ParentTools = ParentTools {
        review_gate: false,
        agent_mailboxes_enabled: false,
        subtle_subagents: true,
    };

    const STRAIGHTFORWARD_ONLY: ParentTools = ParentTools {
        review_gate: false,
        agent_mailboxes_enabled: true,
        subtle_subagents: false,
    };

    fn spawn_definition(options: ParentTools) -> Value {
        tool_definitions_with_options(None, options)
            .into_iter()
            .find(|tool| tool["name"] == "spawn")
            .unwrap()
    }

    #[test]
    fn spawn_offering_both_task_types_keeps_its_description() {
        let spawn = spawn_definition(ParentTools::default());
        assert_eq!(
            spawn["description"],
            "Start a child in your target and filesystem using the task type's configured model and effort. Set task_type to `straightforward` for work with a clear method, such as locating code, running commands or test suites, or implementing a design you have settled. Use `subtle` for work that needs judgment, such as an ambiguous diagnosis, a design choice, or a change with high risk of subtle bugs. A subtle task gets a smarter, more expensive agent. Judge by difficulty, not by the length of the brief. The user configures the model and effort for each type. Returns child_session_id and report_dir immediately; registration does not mean startup succeeded. Reports and startup errors arrive as messages from Mjolnir; use list_agents for status. Reports are short and point to files in report_dir. Supply the assignment and file, symbol, line-range or earlier-report pointers in instructions. Use send_message for follow-up work when an idle child's context helps. Mjolnir selects an enabled profile with the most quota, API-priced profiles last, supporting the task type's configured model and effort. An unavailable model, effort or login is an error, never replaced by another selection. Only children holding processes count against the live children limit; finished children are parked. Close children you no longer need, including failed children after reading the error. Failed-start cleanup must finish before a replacement can use its slot. A child in error cannot be re-prompted. A login the provider has refused stays unavailable until repaired with `mj login`."
        );
        assert_eq!(
            spawn["inputSchema"]["properties"]["task_type"]["enum"],
            json!(["straightforward", "subtle"])
        );
    }

    #[test]
    fn spawn_with_only_straightforward_offers_no_task_type() {
        let spawn = spawn_definition(STRAIGHTFORWARD_ONLY);
        let description = spawn["description"].as_str().unwrap();
        assert!(!description.contains("task_type"), "{description}");
        assert!(!description.contains("`subtle`"), "{description}");
        assert!(!description.contains("task type"), "{description}");
        assert!(
            description.starts_with(
                "Start a child in your target and filesystem using the configured model and effort. Delegate straightforward work: "
            ),
            "{description}"
        );
        assert!(
            description.contains("Do work that needs judgment yourself"),
            "{description}"
        );
        assert!(
            spawn["inputSchema"]["properties"]
                .get("task_type")
                .is_none()
        );
        assert_eq!(
            spawn["inputSchema"]["required"],
            json!(["task_name", "instructions"])
        );
    }

    #[test]
    fn only_list_agents_is_read_only() {
        let read_only = tool_definitions(None)
            .into_iter()
            .filter(|tool| tool["annotations"]["readOnlyHint"] == true)
            .map(|tool| tool["name"].clone())
            .collect::<Vec<_>>();
        assert_eq!(read_only, vec![json!("list_agents")]);
    }

    #[test]
    fn handback_is_read_only_so_plan_mode_reviewers_can_report() {
        let tools = child_tool_definitions();
        assert_eq!(tools[0]["name"], "handback");
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn spawn_offering_both_task_types_requires_one() {
        let error = call(
            Path::new("unused.sock"),
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name":"spawn", "arguments":{"task_name":"t", "instructions":"i"}})),
            &crate::mcp_stdio::Progress::silent(Duration::from_secs(1)),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("missing field `task_type`"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_with_only_straightforward_sends_straightforward() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("subagents.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: SubagentToolRequest = serde_json::from_str(&line).unwrap();
            assert!(matches!(
                request.action,
                SubagentToolAction::Spawn {
                    task_type: mj_core::subagent::SubagentTaskType::Straightforward,
                    ..
                }
            ));
            reader
                .into_inner()
                .write_all(b"{\"result\":{\"child_session_id\":\"child\"}}\n")
                .unwrap();
        });
        let (value, is_error) = call_with_budget_and_options(
            &socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name":"spawn", "arguments":{"task_name":"t", "instructions":"i"}})),
            &crate::mcp_stdio::Progress::silent(Duration::from_secs(1)),
            |_| Duration::from_secs(5),
            STRAIGHTFORWARD_ONLY,
        )
        .unwrap();
        worker.join().unwrap();
        assert!(!is_error, "{value}");
        assert_eq!(value["child_session_id"], "child");
    }

    #[cfg(unix)]
    #[test]
    fn worker_reply_acknowledgment_follows_the_complete_large_reply() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("subagents.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let report = "large report λ".repeat(10_000);
        let expected = report.clone();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let body = json!({"accepted":true,"acknowledgment_required":true,"result":{
                "request_id":request["request_id"],"completed_at_ms":2,"is_error":false,
                "message":json!({"report":report}).to_string()
            }})
            .to_string()
                + "\n";
            reader.get_mut().write_all(body.as_bytes()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            line.clear();
            reader.read_line(&mut line).unwrap();
            let acknowledgment: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(acknowledgment["acknowledge_result"], request["request_id"]);
            reader
                .get_mut()
                .write_all(b"{\"accepted\":true}\n")
                .unwrap();
        });
        let (reply, error) = call_with_budget(
            &socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({"name":"list_agents"})),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_secs(5),
        )
        .unwrap();
        worker.join().unwrap();
        assert!(!error);
        assert_eq!(reply["report"], expected);
    }

    #[test]
    fn review_gate_is_offered_only_to_flagged_parents() {
        for role in [
            SubagentMcpRole::Parent,
            SubagentMcpRole::Child,
            SubagentMcpRole::MessageOnly,
        ] {
            for review_gate in [false, true] {
                let input = format!(
                    "{}\n{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
                    json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
                );
                let output = Arc::new(Mutex::new(Vec::new()));
                run_with_options(
                    input.as_bytes(),
                    ContractWriter(output.clone()),
                    Path::new("unused.sock"),
                    Some(HarnessKind::Claude),
                    role,
                    ParentTools {
                        review_gate,
                        ..Default::default()
                    },
                )
                .unwrap();
                let output = output.lock().unwrap();
                let replies: Vec<Value> = std::str::from_utf8(&output)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                let expected = role == SubagentMcpRole::Parent && review_gate;
                let tools = replies.iter().find(|r| r["id"] == 2).unwrap()["result"]["tools"]
                    .as_array()
                    .unwrap();
                assert_eq!(
                    tools.iter().any(|tool| tool["name"] == "request_review"),
                    expected
                );
                let instructions =
                    replies.iter().find(|r| r["id"] == 1).unwrap()["result"]["instructions"]
                        .as_str()
                        .unwrap();
                assert_eq!(instructions.contains(REVIEW_GATE_INSTRUCTIONS), expected);
                if expected {
                    assert!(instructions.chars().count() <= 2048);
                }
                if !expected {
                    assert!(
                        call_with_budget_and_options(
                            Path::new("unused.sock"),
                            None,
                            role,
                            Some(&json!({"name":"request_review"})),
                            &crate::mcp_stdio::Progress::silent(Duration::from_millis(1)),
                            |_| Duration::from_millis(1),
                            ParentTools {
                                review_gate,
                                ..Default::default()
                            }
                        )
                        .is_err()
                    );
                }
            }
        }
    }

    #[test]
    fn reviewer_mcp_instructions_preserve_the_full_rubric_budget() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("launch.json"),
            br#"{"reviewer_launch":{},"environment":{}}"#,
        )
        .unwrap();
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        run(
            input.as_bytes(),
            ContractWriter(output.clone()),
            &root.path().join("subagents.sock"),
            Some(HarnessKind::Claude),
            SubagentMcpRole::Child,
        )
        .unwrap();
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(output.contains(&mj_core::subagent::MAX_REVIEWER_HANDBACK_CHARS.to_string()));
        assert!(!output.contains("4,000"));
        assert!(output.contains("complete rubric JSON"));
    }

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
            SubagentMcpRole::Parent,
            SubagentMcpRole::MessageOnly,
            SubagentMcpRole::Child,
        ] {
            for harness in [HarnessKind::Claude, HarnessKind::Codex, HarnessKind::Dsh] {
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
                match role {
                    SubagentMcpRole::Child => {
                        assert!(!instructions.contains(DELEGATION_ROUTING));
                        assert!(instructions.contains(mj_core::subagent::HANDBACK_REPORT_RULES));
                        assert_eq!(
                            tools
                                .iter()
                                .map(|tool| tool["name"].as_str().unwrap())
                                .collect::<Vec<_>>(),
                            ["handback"]
                        );
                    }
                    SubagentMcpRole::MessageOnly => {
                        assert!(!instructions.contains(DELEGATION_ROUTING));
                        assert_eq!(
                            tools
                                .iter()
                                .map(|tool| tool["name"].as_str().unwrap())
                                .collect::<Vec<_>>(),
                            ["send_message"]
                        );
                        let send_message = &tools[0];
                        assert_eq!(
                            send_message["inputSchema"]["required"],
                            json!(["session_id", "message"])
                        );
                        assert_eq!(send_message["inputSchema"]["additionalProperties"], false);
                        let description = send_message["description"].as_str().unwrap();
                        for behavior in [
                            "another top-level session",
                            "stopped (suspended)",
                            "full session ID",
                        ] {
                            assert!(description.contains(behavior), "{description}");
                        }
                        assert!(!description.contains("child_session_id"));
                    }
                    SubagentMcpRole::Parent => {
                        assert!(instructions.contains(DELEGATION_ROUTING));
                        assert!(
                            instructions.contains(
                                "Before spawning, give follow-up work through send_message"
                            )
                        );
                        assert_eq!(
                            instructions.contains("Call mj-agents tools directly"),
                            harness == HarnessKind::Codex
                        );
                        let spawn = tools.iter().find(|t| t["name"] == "spawn").unwrap();
                        let props = &spawn["inputSchema"]["properties"];
                        assert!(props.get("files").is_none() && props.get("context").is_none());
                        let mut keys = props
                            .as_object()
                            .unwrap()
                            .keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>();
                        keys.sort();
                        assert_eq!(
                            keys,
                            [
                                "instructions",
                                "task_name",
                                "task_type",
                                "working_directory"
                            ]
                        );
                        assert_eq!(
                            props["task_type"]["enum"],
                            json!(["straightforward", "subtle"])
                        );
                        assert!(
                            spawn["description"]
                                .as_str()
                                .unwrap()
                                .contains("Judge by difficulty, not by the length of the brief")
                        );
                        assert_eq!(spawn["inputSchema"]["additionalProperties"], false);
                        assert_eq!(
                            tools
                                .iter()
                                .filter(|tool| tool["name"] == "send_message")
                                .count(),
                            1
                        );
                        assert!(!tools.iter().any(|tool| tool["name"] == "send_input"));
                        assert!(!tools.iter().any(|t| t["name"] == "interrupt"));
                        let send_message =
                            tools.iter().find(|t| t["name"] == "send_message").unwrap();
                        assert_eq!(
                            send_message["inputSchema"]["required"],
                            json!(["session_id", "message"])
                        );
                        assert_eq!(send_message["inputSchema"]["additionalProperties"], false);
                        let description = send_message["description"].as_str().unwrap();
                        for behavior in [
                            "another top-level session",
                            "stopped (suspended)",
                            "next tool boundary",
                            "without turn cancellation",
                            "idle recipient wakes",
                            "queues a turn",
                            "full session ID",
                        ] {
                            assert!(description.contains(behavior), "{description}");
                        }
                        assert!(!description.contains("child_session_id"));
                    }
                }
            }
        }
    }

    #[test]
    fn send_message_rejects_the_old_public_target_name() {
        let error = call_with_budget(
            Path::new("unused.sock"),
            None,
            SubagentMcpRole::Parent,
            Some(&json!({
                "name":"send_message",
                "arguments":{"child_session_id":"peer","message":"hello"}
            })),
            &crate::mcp_stdio::Progress::silent(Duration::from_secs(1)),
            |_| Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "unknown parameter `child_session_id`; use `session_id`"
        );
    }

    #[test]
    fn legacy_mailbox_flag_keeps_peer_tools_and_instructions_available() {
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        );
        for role in [SubagentMcpRole::Parent, SubagentMcpRole::MessageOnly] {
            let output = Arc::new(Mutex::new(Vec::new()));
            run_with_options(
                input.as_bytes(),
                ContractWriter(output.clone()),
                Path::new("unused.sock"),
                Some(HarnessKind::Codex),
                role,
                MAILBOXES_DISABLED,
            )
            .unwrap();
            let output = output.lock().unwrap();
            let replies: Vec<Value> = std::str::from_utf8(&output)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let tools = replies.iter().find(|reply| reply["id"] == 2).unwrap()["result"]["tools"]
                .as_array()
                .unwrap();
            assert!(tools.iter().any(|tool| tool["name"] == "send_message"));
            let instructions =
                replies.iter().find(|reply| reply["id"] == 1).unwrap()["result"]["instructions"]
                    .as_str()
                    .unwrap();
            assert!(instructions.contains("send_message"));
        }
    }

    #[test]
    fn parent_requires_task_type_and_refuses_hidden_tools_and_selector_overrides() {
        let tools = tool_definitions(None);
        assert!(!tools.iter().any(|tool| tool["name"] == "list_profiles"));
        assert!(tools.iter().any(|tool| tool["name"] == "send_message"));
        assert!(!tools.iter().any(|tool| tool["name"] == "send_input"));
        assert!(!tools.iter().any(|tool| tool["name"] == "interrupt"));
        let spawn = tools.iter().find(|tool| tool["name"] == "spawn").unwrap();
        assert_eq!(
            spawn["inputSchema"]["required"],
            json!(["task_name", "instructions", "task_type"])
        );
        let mut calls = vec![json!({"name":"list_profiles", "arguments":{}})];
        for key in ["profile_id", "model", "effort"] {
            assert!(spawn["inputSchema"]["properties"].get(key).is_none());
            let mut args = json!({"task_name":"probe", "instructions":"report", "task_type":"straightforward"});
            args[key] = json!("override");
            calls.push(json!({"name":"spawn", "arguments":args}));
        }
        for params in calls {
            let error = call(
                Path::new("unused.sock"),
                None,
                SubagentMcpRole::Parent,
                Some(&params),
                &crate::mcp_stdio::Progress::silent(Duration::from_secs(1)),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("unknown sub-agent tool")
                    || error.to_string().contains("unknown field"),
                "{error:#}"
            );
        }
    }

    /// #1160: a parent read a child's failed login as the child's report and
    /// gave up on the profile. The tools say what a refused login looks like
    /// and what to do about it.
    // Hard-won: #1160: a refused provider login was mistaken for a child report.
    #[test]
    fn list_and_spawn_say_what_a_refused_login_looks_like() {
        let description = |name: &str| {
            tool_definitions(None)
                .into_iter()
                .find(|tool| tool["name"] == name)
                .and_then(|tool| tool["description"].as_str().map(str::to_owned))
                .unwrap()
        };
        let wait = description("list_agents");
        assert!(wait.contains("failure kind login_invalid"), "{wait}");
        assert!(wait.contains("`mj login`"), "{wait}");
        assert!(wait.contains("not about the task"), "{wait}");
        let spawn = description("spawn");
        assert!(spawn.contains("login the provider has refused"), "{spawn}");
    }

    /// #1161: a child that handed back is parked. The parent has to know
    /// that such a child holds no processes, that `send_message` starts it
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
        let send_message = description("send_message");
        assert!(send_message.contains("unique case-insensitive hexadecimal prefix"));
        assert!(send_message.contains("at least 8 characters"));
        assert!(send_message.contains("resolved full session ID"));
        assert!(
            send_message.contains("parked child is restarted"),
            "{send_message}"
        );
        assert!(
            send_message.contains("parked child starts a turn"),
            "{send_message}"
        );
        assert!(send_message.contains("tens of seconds"), "{send_message}");
        let wait = description("list_agents");
        assert!(wait.contains("parked true"), "{wait}");
        let spawn = description("spawn");
        assert!(spawn.contains("live children"), "{spawn}");
        assert!(spawn.contains("finished children are parked"), "{spawn}");
        assert!(SERVER_INSTRUCTIONS.contains("Finished children are parked"));
        assert!(SERVER_INSTRUCTIONS.contains("send_message resumes one"));
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
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("launch.json"),
            serde_json::to_vec(&json!({"environment": {
                (mj_core::subagent::CHILD_REPORT_DIR_ENV): directory.path().join("reports"),
                (mj_core::subagent::CHILD_REPORT_LIMIT_ENV): "11184810"
            }}))
            .unwrap(),
        )
        .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        run(
            input.as_bytes(),
            SharedWriter(Arc::clone(&output)),
            &directory.path().join("subagents.sock"),
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
        assert!(
            handback["description"]
                .as_str()
                .unwrap()
                .contains("Report directory limit: 11184810 bytes")
        );
        assert!(handback["description"].as_str().unwrap().len() <= 1800);
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
            Some(&json!({"name": "spawn", "arguments": {"task_name": "t", "instructions": "i", "task_type":"straightforward"}})),
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
            Some(&json!({"name": "spawn", "arguments": {"task_name": "t", "instructions": "i", "task_type":"straightforward"}})),
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

    // Hard-won: #1331: binary artifacts silently displaced report evidence in checkpoints.
    #[test]
    fn handback_refuses_oversized_reports_and_lists_the_ten_largest_files() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("reports");
        std::fs::create_dir_all(reports.join("nested")).unwrap();
        for size in 1..=12 {
            std::fs::File::create(reports.join("nested").join(format!("artifact-{size:02}")))
                .unwrap()
                .set_len(size)
                .unwrap();
        }
        let socket = dir.path().join("subagents.sock");
        std::fs::write(
            dir.path().join("launch.json"),
            serde_json::to_vec(&json!({
                "environment": {
                    (mj_core::subagent::CHILD_REPORT_DIR_ENV): reports,
                    (mj_core::subagent::CHILD_REPORT_LIMIT_ENV): "77"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let (value, is_error) = call(
            &socket,
            None,
            SubagentMcpRole::Child,
            Some(&json!({"name":"handback", "arguments":{"message":"done"}})),
            &crate::mcp_stdio::Progress::silent(Duration::from_secs(1)),
        )
        .unwrap();
        assert!(is_error, "{value}");
        let error = value["error"].as_str().unwrap();
        assert!(
            error.contains("holds 78 bytes; limit is 77 bytes"),
            "{error}"
        );
        let files = error
            .lines()
            .filter(|line| line.starts_with("  "))
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 10, "{error}");
        assert!(
            files[0].contains("12 bytes: nested") && files[0].ends_with("artifact-12"),
            "{error}"
        );
        assert!(files[9].ends_with("artifact-03"), "{error}");
        assert!(!error.contains("artifact-02"), "{error}");
        assert!(
            error.contains("Delete build outputs, binaries and downloads"),
            "{error}"
        );
        assert!(error.contains("call handback again"), "{error}");
    }

    #[test]
    fn older_launch_files_without_both_report_settings_skip_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        assert!(ChildReportBudget::from_socket(&socket).unwrap().is_none());
        for environment in [
            json!({}),
            json!({
                (mj_core::subagent::CHILD_REPORT_DIR_ENV): "/missing"
            }),
            json!({
                (mj_core::subagent::CHILD_REPORT_LIMIT_ENV): "1"
            }),
        ] {
            std::fs::write(
                dir.path().join("launch.json"),
                serde_json::to_vec(&json!({"environment":environment})).unwrap(),
            )
            .unwrap();
            assert!(ChildReportBudget::from_socket(&socket).unwrap().is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn report_budget_does_not_follow_file_or_directory_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("reports");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&reports).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::File::create(outside.join("large"))
            .unwrap()
            .set_len(1_000_000)
            .unwrap();
        std::os::unix::fs::symlink(&outside, reports.join("directory-link")).unwrap();
        std::os::unix::fs::symlink(outside.join("large"), reports.join("file-link")).unwrap();
        std::os::unix::fs::symlink(outside.join("missing"), reports.join("broken-link")).unwrap();
        ChildReportBudget {
            directory: reports,
            limit: 0,
        }
        .check()
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_child_sends_its_report_as_a_handback_request() {
        for configured in [false, true] {
            child_sends_its_report(configured);
        }
    }

    #[cfg(unix)]
    fn child_sends_its_report(configured: bool) {
        use std::io::{BufReader, Read};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("subagents.sock");
        if configured {
            let reports = dir.path().join("reports");
            std::fs::create_dir(&reports).unwrap();
            std::fs::write(reports.join("evidence.log"), "evidence").unwrap();
            std::fs::write(
                dir.path().join("launch.json"),
                serde_json::to_vec(&json!({
                    "environment": {
                        (mj_core::subagent::CHILD_REPORT_DIR_ENV): reports,
                        (mj_core::subagent::CHILD_REPORT_LIMIT_ENV): "8"
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }
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
    fn send_message_accepts_the_unlisted_send_input_alias() {
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
        let (value, is_error) = call_with_budget_and_options(
            &socket,
            None,
            SubagentMcpRole::Parent,
            Some(&json!({
                "name":"send_input",
                "arguments":{"child_session_id":"child","message":"keep going"}
            })),
            &crate::mcp_stdio::Progress::silent(Duration::from_millis(50)),
            |_| Duration::from_secs(5),
            MAILBOXES_DISABLED,
        )
        .unwrap();
        assert!(!is_error, "{value}");
        assert_eq!(value["status"], "queued");
        let request = received.recv().unwrap();
        assert_eq!(
            request["action"],
            json!({
                "action":"send_input",
                "params":{"child_session_id":"child","message":"keep going"}
            })
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
                "task_name": "t", "instructions": "i", "task_type":"straightforward", "request_key": "k"
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
        // One receiver, taken by whichever connection carries the `spawn`.
        let answered = Arc::new(Mutex::new(answered));

        // Fake worker: reply to any non-spawn call at once, and hold the
        // `spawn` reply until the cheap call has been answered. Each
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
                    if action == "spawn" {
                        answered
                            .lock()
                            .expect("handshake receiver poisoned")
                            .recv()
                            .expect("the cheap call must be answered while the spawn is open");
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

        // The slow `spawn` is sent first, the cheap `list_agents` second. Only
        // concurrent dispatch can answer the cheap call first, because the
        // `spawn`'s reply is released by that answer being written. A dispatcher
        // that ran the calls in order would answer the `spawn` first — nothing
        // releases it, so it would fall back to its own budget — and the two
        // responses would come back the other way round.
        let input = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"spawn","arguments":{"task_name":"slow","instructions":"work","task_type":"straightforward"}}}),
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
            "the cheap list_agents response must be written before the slow spawn: {text}"
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
            next.contains("list_agents")
                && next.contains("stopped")
                && next.contains("replacement"),
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
}
