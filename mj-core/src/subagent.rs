//! Durable contracts for Mjolnir-managed child-agent sessions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Longest time a sub-agent completion wait may remain pending.
pub const MAX_WAIT_SECONDS: u64 = 3_600;

/// How long a `wait` call blocks when the caller gives no timeout: as long as
/// the caller's harness allows, since every `wait` call costs the parent a
/// request carrying its whole context. [`subagent_wait_timeout_for`] caps it at
/// the harness's own ceiling.
pub const DEFAULT_WAIT_SECONDS: u64 = MAX_WAIT_SECONDS;

/// The `spawn` model value that means "the model the parent is running now".
pub const CURRENT_MODEL: &str = "current";

/// `status` when every named child finished its turn before the deadline.
pub const WAIT_STATUS_COMPLETE: &str = "complete";

/// `status` when the deadline arrived first. It is an answer, not a failure:
/// the children are still working and the caller collects them by calling
/// `wait` again.
pub const WAIT_STATUS_STILL_RUNNING: &str = "still_running";

/// The longest `wait` a Codex parent can be given. Codex abandons a `tools/call`
/// after 300 seconds of total elapsed time, and progress notifications do not
/// reset that timer, so a Codex parent's wait has to fit underneath it: measured
/// on 2026-09-18 with `codex` 0.60.0, where a call asking for 3600 seconds came
/// back at 303 seconds with "timed out awaiting tools/call after 300s". The
/// answer must be written well before that, so the cap leaves room for the
/// worker's five-second grace and the trip back.
///
/// Claude Code's limit is on silence rather than on elapsed time, and the
/// progress notifications this server sends break that silence, so a Claude
/// parent keeps the full [`MAX_WAIT_SECONDS`].
pub const MAX_CODEX_WAIT_SECONDS: u64 = 240;

/// The answer to a capped wait, plus every grace on top of it, must still be
/// written before the client gives up.
const _: () = assert!(MAX_CODEX_WAIT_SECONDS + 30 < 300);

/// The longest single `wait` this harness's own MCP client will hold open.
pub fn max_wait_seconds_for(harness: Option<crate::config::HarnessKind>) -> u64 {
    match harness {
        Some(crate::config::HarnessKind::Codex) => MAX_CODEX_WAIT_SECONDS,
        _ => MAX_WAIT_SECONDS,
    }
}

/// How long one `wait` call blocks, from what the caller asked for. The shim,
/// the worker and the daemon all resolve the caller's request through this one
/// function so the three cannot disagree about when the answer is due. The
/// harness-specific ceiling is applied once, where the request is built, so the
/// three see the same number.
pub fn subagent_wait_timeout(requested: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_secs(
        requested
            .unwrap_or(DEFAULT_WAIT_SECONDS)
            .clamp(1, MAX_WAIT_SECONDS),
    )
}

/// What a `wait` from this harness may actually ask for.
pub fn subagent_wait_timeout_for(
    harness: Option<crate::config::HarnessKind>,
    requested: Option<u64>,
) -> std::time::Duration {
    let ceiling = max_wait_seconds_for(harness);
    std::time::Duration::from_secs(requested.unwrap_or(DEFAULT_WAIT_SECONDS).clamp(1, ceiling))
}

/// What is left of a `wait` call's budget, counted from when the caller made
/// the request rather than from when work on it started. A request that is
/// executed again after a daemon restart therefore still answers at the
/// caller's original deadline instead of starting its timeout over.
///
/// The two clocks involved can belong to different hosts, so the elapsed time
/// is clamped into `0..=requested`: skew can neither extend a wait past what
/// the caller asked for nor turn it negative.
pub fn remaining_subagent_wait(
    created_at_ms: i64,
    requested: Option<u64>,
    now_ms: i64,
) -> std::time::Duration {
    let budget = subagent_wait_timeout(requested);
    let elapsed_ms = now_ms.saturating_sub(created_at_ms).max(0) as u64;
    budget.saturating_sub(std::time::Duration::from_millis(elapsed_ms))
}

/// The answer to a `wait` whose deadline arrived before the children finished,
/// for callers that know only which children were asked about. The daemon
/// builds a richer version of this shape with each child's own state; this one
/// is what the worker answers with when the daemon itself was late.
pub fn still_running_payload(
    child_session_ids: &[String],
    waited_seconds: u64,
    note: Option<&str>,
) -> serde_json::Value {
    let agents = child_session_ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "child_session_id": id,
                "state": "unknown",
                "finished": false,
                "output": serde_json::Value::Null,
            })
        })
        .collect::<Vec<_>>();
    let mut payload = serde_json::json!({
        "status": WAIT_STATUS_STILL_RUNNING,
        "waited_seconds": waited_seconds,
        "agents": agents,
        "next_action": next_action(child_session_ids, child_session_ids),
    });
    if let Some(note) = note
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("note".into(), serde_json::Value::String(note.into()));
    }
    payload
}

/// The one sentence that tells the model what to do with this answer. It is
/// part of the answer rather than of the tool description because a model
/// reads the answer it just got far more reliably than a schema it read once.
///
/// `unfinished` names the children still running out of the `total` asked
/// about. When some finished, the sentence names the ones left to wait for, so
/// a parent does not collect the finished children's reports again.
pub fn next_action(total: &[String], unfinished: &[String]) -> String {
    if unfinished.is_empty() {
        return "All children finished. Their reports are in each agent's output field.".to_owned();
    }
    if unfinished.len() == total.len() {
        return format!(
            "{} of {} child sessions are still running; this is not a failure. \
             Call wait again with the same child_session_ids to keep waiting, \
             or do other work first and call wait later.",
            unfinished.len(),
            total.len()
        );
    }
    format!(
        "{} of {} child sessions finished; their reports are in output. The others are still \
         running, which is not a failure. Call wait with the remaining child_session_ids to \
         keep waiting: {}.",
        total.len() - unfinished.len(),
        total.len(),
        unfinished.join(", ")
    )
}

/// An inclusive, one-based line range within a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineRange {
    pub start: u64,
    pub end: u64,
}

/// One or more line ranges captured from a single file for a child's initial
/// context. Grouping by file lets a parent pull several disjoint ranges out of
/// the same file in one entry, rather than one range per file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSourceRanges {
    pub file: PathBuf,
    pub ranges: Vec<LineRange>,
}

/// One MCP request created inside a parent worker and consumed by the
/// controller. `request_id` is the idempotency identity across reconnects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentToolRequest {
    pub request_id: String,
    pub created_at_ms: i64,
    pub action: SubagentToolAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SubagentToolAction {
    ListProfiles,
    Spawn {
        task_name: String,
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        /// Absolute, or relative to the parent session's working directory.
        /// Empty means the parent's own working directory. The directory must
        /// exist on the target; no other restriction applies.
        #[serde(default)]
        working_directory: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<FileSourceRanges>,
    },
    ListAgents,
    SendInput {
        child_session_id: String,
        message: String,
    },
    WaitAgents {
        child_session_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,
        /// Left off the wire when it is the default, so a wait for every
        /// child reads exactly as it did before the field existed.
        #[serde(default, skip_serializing_if = "ReturnWhen::is_all")]
        return_when: ReturnWhen,
    },
    InterruptAgent {
        child_session_id: String,
    },
    CloseAgent {
        child_session_id: String,
    },
    /// A child's report for the session that started it. Only a child's
    /// worker serves this action; the daemon runs it for the requesting child.
    Handback {
        message: String,
    },
}

/// When a `wait` answers before its timeout: once every named child finished,
/// or once any one of them did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnWhen {
    #[default]
    All,
    Any,
}

impl ReturnWhen {
    #[must_use]
    pub fn is_all(&self) -> bool {
        *self == Self::All
    }

    /// Whether a wait with this rule is answered, given which children
    /// finished. A wait naming no children has nothing to wait for.
    #[must_use]
    pub fn satisfied(self, finished: &[bool]) -> bool {
        match self {
            Self::All => finished.iter().all(|done| *done),
            Self::Any => finished.is_empty() || finished.iter().any(|done| *done),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentToolResult {
    pub request_id: String,
    pub completed_at_ms: i64,
    pub is_error: bool,
    pub message: String,
}

/// Durable ownership and launch intent for one child session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentRecord {
    pub child_session_id: String,
    pub parent_session_id: String,
    pub task_name: String,
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Launch directory for the child on the parent's target: absolute, or
    /// relative to the parent session's working directory. Empty means the
    /// parent's own working directory. The directory must exist; no other
    /// restriction applies.
    pub working_directory: PathBuf,
    /// Complete first prompt after the controller captures requested ranges.
    pub initial_prompt: String,
    pub request_key: String,
    pub created_at: String,
    /// The child turn whose completion notice the parent's transcript has
    /// already recorded, so restarts do not repeat the notice. Stored under
    /// the historical field name `delivered_turn`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "delivered_turn"
    )]
    pub noticed_turn: Option<u64>,
    /// Whether this child was given the `handback` tool. It is decided once,
    /// when the child is registered. A child recorded before the tool existed
    /// reads as false and keeps reporting through its last message.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub handback_tool: bool,
}

/// Lifecycle group used by the tool and both user interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Preparing,
    Running,
    InputRequired,
    Completed,
    Failed,
    Interrupted,
    Stopped,
}

impl SubagentRecord {
    #[must_use]
    pub fn is_child(&self, session_id: &str) -> bool {
        self.child_session_id == session_id
    }
}

/// The name a worker's sub-agent MCP server is registered under in every
/// harness, so its tools reach the model as `mcp__mj-agents__<tool>`.
pub const SUBAGENT_MCP_SERVER: &str = "mj-agents";

/// Which tools a worker's `mj-agents` MCP server offers. A parent delegates;
/// a child only hands its report back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentMcpRole {
    #[default]
    Parent,
    Child,
}

impl SubagentMcpRole {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Child => "child",
        }
    }

    /// The tools this role's server lists, in the order it lists them. A
    /// harness that asks before an MCP tool is allowed exactly these.
    #[must_use]
    pub fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::Parent => &[
                "list_profiles",
                "spawn",
                "list_agents",
                "send_input",
                "wait",
                "interrupt",
                "close",
            ],
            Self::Child => &["handback"],
        }
    }
}

impl std::fmt::Display for SubagentMcpRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.id())
    }
}

impl std::str::FromStr for SubagentMcpRole {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "parent" => Ok(Self::Parent),
            "child" => Ok(Self::Child),
            other => anyhow::bail!("unknown sub-agent MCP role {other:?}"),
        }
    }
}

/// Command id prefix of the one prompt that reminds a child to hand back its
/// report. A turn whose command id carries it is that reminder, which is how
/// [`report_state`] tells it from the task it follows up.
pub const HANDBACK_REMINDER_PREFIX: &str = "handback-reminder";

/// The reminder itself, sent as an ordinary prompt so the child's transcript
/// shows it.
pub const HANDBACK_REMINDER_TEXT: &str = "[handback reminder] Your report has not been delivered. Call the mj-agents handback tool now with your full report, then stop.";

/// What a child's report must contain and what it must leave to files. Shared
/// by the first-prompt note, the child's server instructions and the handback
/// tool description, so the three cannot drift apart.
pub const HANDBACK_REPORT_RULES: &str = "Your report is what the parent reads, and it is at most 4,000 characters: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Write anything longer (logs, tables, full findings) to files in your report directory and list their paths in the report; never put it in the report itself. Fix mechanical errors yourself (compile errors, lint and format findings, test failures your own change caused) and list each fix in one line of your report. Hand back a question and stop when the fix needs a design choice, touches a file you were not given, changes a persisted identity, a public contract or an epoch, or the failure also occurs on the base commit.";

/// What a child's first prompt opens with when it has the tool. It leads the
/// prompt rather than trailing a parent's long instructions, where live runs
/// showed children skipping it. `report_dir` is the directory Mjolnir created
/// for this child's files.
#[must_use]
pub fn handback_prompt_note(report_dir: &str) -> String {
    format!(
        "You are a Mjolnir sub-agent. Finish every task by calling the mj-agents handback tool with your report: the session that started you reads only that report, not the rest of this conversation. Your report directory is {report_dir}. {HANDBACK_REPORT_RULES}"
    )
}

/// How long a sent reminder may be neither queued, running nor finished before
/// the child's report stops waiting for it. Someone removed it from the queue,
/// or it never reached the child; either way it is not coming.
pub const HANDBACK_REMINDER_GRACE_MS: i64 = 30_000;

/// The longest report a child can hand back, and the longest output a `wait`
/// shows for any child. Details belong in files in the child's report
/// directory; the parent reads this on every later request it makes.
pub const MAX_HANDBACK_CHARS: usize = 4_000;

/// Directory under a parent's workspace root that holds its children's report
/// directories, outside every repository.
pub const REPORT_ROOT_DIR: &str = ".mj-agents";

/// For a session whose workspace root is not Mjolnir's (a bare project), the
/// report root sits inside the project, under a path its `info/exclude` lists.
pub const PROJECT_REPORT_ROOT_DIR: &str = ".mj/agents";

/// Archive prefix for the report files a checkpoint carries.
pub const ARCHIVE_REPORT_DIR: &str = "mj-agent-reports";

/// Cut `text` to [`MAX_HANDBACK_CHARS`] characters, saying how much was left
/// out and how the parent can get it. Returns the text and whether it was cut.
#[must_use]
pub fn bounded_report(text: &str) -> (String, bool) {
    let total = text.chars().count();
    if total <= MAX_HANDBACK_CHARS {
        return (text.to_owned(), false);
    }
    let kept: String = text.chars().take(MAX_HANDBACK_CHARS).collect();
    (
        format!(
            "{kept}\n[truncated: {} more characters; use send_input to ask the child to write the details to files in its report directory and send you the paths]",
            total - MAX_HANDBACK_CHARS
        ),
        true,
    )
}

/// Whether a command id names a handback reminder.
#[must_use]
pub fn is_handback_reminder(command_id: &str) -> bool {
    command_id
        .strip_prefix(HANDBACK_REMINDER_PREFIX)
        .is_some_and(|rest| rest.starts_with('-'))
}

/// A report a child handed back during the turn `command_id` names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentHandback {
    pub command_id: String,
    pub message: String,
    pub recorded_at_ms: i64,
}

/// The reminder Mjolnir sent after the turn `for_command_id` ended without a
/// report. `command_id` is the reminder prompt's own command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandbackReminder {
    pub command_id: String,
    pub for_command_id: String,
    pub sent_at_ms: i64,
}

/// Everything recorded about one child's report. Each part keeps only its
/// latest value: a report answers the newest task, and each task gets at most
/// one reminder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentReport {
    pub handback: Option<SubagentHandback>,
    pub reminder: Option<HandbackReminder>,
    /// The turn whose reminder could not be sent.
    pub reminder_failed_for: Option<String>,
    /// Acceptance ordinal of the newest prompt the parent gave this child.
    /// The child has not answered it until a finished turn reaches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaited_ordinal: Option<u64>,
    /// The absolute directory on the parent's target where this child writes
    /// the details its report points to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_dir: Option<String>,
}

/// Whether the parent's newest prompt has yet to be answered: no finished
/// turn has reached its acceptance ordinal. The store learns of a new turn a
/// moment after the prompt is accepted, and in that gap an idle child would
/// otherwise read as finished with the previous turn's report, or none.
#[must_use]
pub fn awaiting_prompt(
    awaited_ordinal: Option<u64>,
    last_turn: Option<&crate::state::MaterializedTurnOutcome>,
) -> bool {
    awaited_ordinal.is_some_and(|awaited| {
        last_turn
            .and_then(|turn| turn.accepted_ordinal)
            .is_none_or(|answered| answered < awaited)
    })
}

/// How a child's last finished turn failed, if it did: `interrupted` for a
/// cancelled or interrupted turn, `failed` for an error, a refusal or a quota
/// stop, with the most specific reason recorded. A turn that finished or
/// stopped to ask for input did not fail.
#[must_use]
pub fn failed_turn(
    turn: &crate::state::MaterializedTurnOutcome,
    last_message: Option<&str>,
) -> Option<(&'static str, String)> {
    use crate::state::{PromptCompletion, TurnOutcomeKind, classify_prompt_completion};
    let (state, fallback) = match &turn.outcome {
        TurnOutcomeKind::Completed { stop_reason } => match classify_prompt_completion(stop_reason)
        {
            PromptCompletion::Finished | PromptCompletion::InputRequired => return None,
            PromptCompletion::Cancelled => ("interrupted", "the turn was cancelled".to_owned()),
            PromptCompletion::QuotaLimit | PromptCompletion::Error => (
                "failed",
                format!("the turn ended with stop reason {stop_reason:?}"),
            ),
        },
        TurnOutcomeKind::Interrupted { message } => ("interrupted", message.clone()),
        TurnOutcomeKind::Rejected { message } => ("failed", message.clone()),
    };
    let reason = turn
        .diagnostic
        .as_ref()
        .map(|diagnostic| diagnostic.message.clone())
        .filter(|message| !message.trim().is_empty())
        .or_else(|| {
            last_message
                .filter(|message| !message.trim().is_empty())
                .map(str::to_owned)
        })
        .unwrap_or(fallback);
    Some((state, reason))
}

/// Where a child's report stands after its last finished turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportState {
    /// The child handed back this report for its last turn.
    Delivered(String),
    /// The child owes a report. `remind` says no reminder has been sent for
    /// this turn yet; otherwise one is on its way.
    Pending { remind: bool },
    /// No handback is coming: report the turn's last message, as before.
    Fallback,
}

/// The one rule for a child's report, shared by the sub-agent `wait`, the
/// session wait and the reminder.
///
/// `in_flight` names the commands queued or running on the child, so a sent
/// reminder is known to be on its way. A child without the tool, and a turn
/// that did not finish normally, fall back to the last message; a child that
/// already had its one reminder does too.
#[must_use]
pub fn report_state(
    handback_tool: bool,
    report: &SubagentReport,
    last_turn: Option<&crate::state::MaterializedTurnOutcome>,
    in_flight: &[&str],
    now_ms: i64,
) -> ReportState {
    let Some(turn) = last_turn.filter(|_| handback_tool) else {
        return ReportState::Fallback;
    };
    if let Some(handback) = report
        .handback
        .as_ref()
        .filter(|handback| handback.command_id == turn.command_id)
    {
        return ReportState::Delivered(handback.message.clone());
    }
    let finished = matches!(
        &turn.outcome,
        crate::state::TurnOutcomeKind::Completed { stop_reason }
            if crate::state::classify_prompt_completion(stop_reason)
                == crate::state::PromptCompletion::Finished
    );
    if !finished
        || is_handback_reminder(&turn.command_id)
        || report.reminder_failed_for.as_deref() == Some(turn.command_id.as_str())
    {
        return ReportState::Fallback;
    }
    match report
        .reminder
        .as_ref()
        .filter(|reminder| reminder.for_command_id == turn.command_id)
    {
        None => ReportState::Pending { remind: true },
        Some(reminder)
            if in_flight.contains(&reminder.command_id.as_str())
                || now_ms.saturating_sub(reminder.sent_at_ms) < HANDBACK_REMINDER_GRACE_MS =>
        {
            ReportState::Pending { remind: false }
        }
        Some(_) => ReportState::Fallback,
    }
}

/// A sub-agent that Mjolnir stopped when its parent was suspended. The
/// parent's record keeps one of these per child until the parent's model has
/// been told, on the first prompt after the parent resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedSubagent {
    pub child_session_id: String,
    /// The child's listed title when it was stopped.
    pub title: String,
    /// One line of the task the parent gave it, when the spawn recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Whether the child had handed back its report for the parent's newest
    /// task; see [`has_handed_back`].
    pub handed_back: bool,
}

/// Longest task line a [`StoppedSubagent`] keeps, in characters.
pub const STOPPED_TASK_CHARS: usize = 160;

/// How [`handback_prompt_note`] begins, whatever directory it names.
const HANDBACK_NOTE_OPENING: &str = "You are a Mjolnir sub-agent. ";

/// One line of the task a parent gave a child, from the child's first prompt:
/// the parent's instructions without the handback note Mjolnir puts before
/// them, and without the context and file ranges the parent attached after
/// them. `report_dir` is the directory that note names, when one was recorded.
#[must_use]
pub fn task_summary(initial_prompt: &str, report_dir: Option<&str>) -> Option<String> {
    let mut task = initial_prompt;
    if let Some(rest) = report_dir.and_then(|directory| {
        task.strip_prefix(handback_prompt_note(directory).as_str())
            .and_then(|rest| rest.strip_prefix("\n\n"))
    }) {
        task = rest;
    } else if task.starts_with(HANDBACK_NOTE_OPENING)
        && let Some((_, rest)) = task.split_once("\n\n")
    {
        task = rest;
    }
    for attachment in ["\n\n<parent_context>", "\n\n--- source "] {
        if let Some((instructions, _)) = task.split_once(attachment) {
            task = instructions;
        }
    }
    let line = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= STOPPED_TASK_CHARS {
        return Some(line);
    }
    let kept: String = line.chars().take(STOPPED_TASK_CHARS - 1).collect();
    Some(format!("{}…", kept.trim_end()))
}

/// Whether a child had handed back its report for the parent's newest task:
/// it is not working, a finished turn answers the parent's newest prompt, and
/// that answer is final. The answer is the report the child handed back, or,
/// for a child without the tool or one that ignored its reminder, the last
/// message of a turn that finished normally. A turn that failed, was
/// interrupted or stopped to ask a question has not handed anything back.
///
/// `working` says the child has a turn running or its execution is not idle.
#[must_use]
pub fn has_handed_back(
    handback_tool: bool,
    report: &SubagentReport,
    working: bool,
    last_turn: Option<&crate::state::MaterializedTurnOutcome>,
    now_ms: i64,
) -> bool {
    let Some(turn) = last_turn else {
        return false;
    };
    if working || awaiting_prompt(report.awaited_ordinal, Some(turn)) {
        return false;
    }
    match report_state(handback_tool, report, Some(turn), &[], now_ms) {
        ReportState::Delivered(_) => true,
        ReportState::Pending { .. } => false,
        ReportState::Fallback => matches!(
            &turn.outcome,
            crate::state::TurnOutcomeKind::Completed { stop_reason }
                if crate::state::classify_prompt_completion(stop_reason)
                    == crate::state::PromptCompletion::Finished
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(command_id: &str, stop_reason: &str) -> crate::state::MaterializedTurnOutcome {
        crate::state::MaterializedTurnOutcome {
            diagnostic: None,
            usage: None,
            command_id: command_id.to_owned(),
            accepted_ordinal: Some(1),
            turn_start_position: Some(1),
            completed_ordinal: 2,
            completed_at_ms: 10,
            outcome: crate::state::TurnOutcomeKind::Completed {
                stop_reason: stop_reason.to_owned(),
            },
        }
    }

    fn handback(command_id: &str) -> Option<SubagentHandback> {
        Some(SubagentHandback {
            command_id: command_id.to_owned(),
            message: "the report".to_owned(),
            recorded_at_ms: 5,
        })
    }

    fn reminder(for_command_id: &str) -> Option<HandbackReminder> {
        Some(HandbackReminder {
            command_id: "handback-reminder-r".to_owned(),
            for_command_id: for_command_id.to_owned(),
            sent_at_ms: 1_000,
        })
    }

    #[test]
    fn a_child_without_the_tool_or_a_turn_reports_its_last_message() {
        let turn = finished("task", "end_turn");
        let report = SubagentReport::default();
        assert_eq!(
            report_state(false, &report, Some(&turn), &[], 0),
            ReportState::Fallback
        );
        assert_eq!(
            report_state(true, &report, None, &[], 0),
            ReportState::Fallback
        );
    }

    #[test]
    fn a_handback_for_the_last_turn_is_the_report_whatever_the_turn_did() {
        let report = SubagentReport {
            handback: handback("task"),
            ..SubagentReport::default()
        };
        for stop_reason in ["end_turn", "cancelled", "refusal"] {
            assert_eq!(
                report_state(true, &report, Some(&finished("task", stop_reason)), &[], 0),
                ReportState::Delivered("the report".to_owned()),
                "{stop_reason}"
            );
        }
        // A report for an earlier task does not answer this one.
        assert_eq!(
            report_state(true, &report, Some(&finished("next", "end_turn")), &[], 0),
            ReportState::Pending { remind: true }
        );
    }

    #[test]
    fn only_a_normally_finished_task_turn_is_owed_a_reminder() {
        let report = SubagentReport::default();
        assert_eq!(
            report_state(true, &report, Some(&finished("task", "end_turn")), &[], 0),
            ReportState::Pending { remind: true }
        );
        for stop_reason in ["cancelled", "awaiting_input", "quota_limit", "max_tokens"] {
            assert_eq!(
                report_state(true, &report, Some(&finished("task", stop_reason)), &[], 0),
                ReportState::Fallback,
                "{stop_reason}"
            );
        }
        let mut interrupted = finished("task", "end_turn");
        interrupted.outcome = crate::state::TurnOutcomeKind::Interrupted {
            message: "stopped".to_owned(),
        };
        assert_eq!(
            report_state(true, &report, Some(&interrupted), &[], 0),
            ReportState::Fallback
        );
    }

    #[test]
    fn a_child_gets_one_reminder_and_then_reports_its_last_message() {
        let reminded = SubagentReport {
            reminder: reminder("task"),
            ..SubagentReport::default()
        };
        let task = finished("task", "end_turn");
        // Sent and queued, or sent a moment ago: it is on its way.
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&task),
                &["handback-reminder-r"],
                60_000
            ),
            ReportState::Pending { remind: false }
        );
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&task),
                &[],
                1_000 + HANDBACK_REMINDER_GRACE_MS - 1
            ),
            ReportState::Pending { remind: false }
        );
        // Neither queued, running nor finished long after it was sent.
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&task),
                &[],
                1_000 + HANDBACK_REMINDER_GRACE_MS
            ),
            ReportState::Fallback
        );
        // The reminder turn itself ended without a report.
        assert_eq!(
            report_state(
                true,
                &reminded,
                Some(&finished("handback-reminder-r", "end_turn")),
                &[],
                2_000
            ),
            ReportState::Fallback
        );
        // A reminder that could not be sent.
        let failed = SubagentReport {
            reminder_failed_for: Some("task".to_owned()),
            ..SubagentReport::default()
        };
        assert_eq!(
            report_state(true, &failed, Some(&task), &[], 0),
            ReportState::Fallback
        );
    }

    #[test]
    fn a_child_is_not_done_until_a_finished_turn_reaches_the_newest_prompt() {
        let mut turn = finished("task", "end_turn");
        turn.accepted_ordinal = Some(20);
        assert!(!awaiting_prompt(None, Some(&turn)));
        assert!(awaiting_prompt(Some(45), None), "no turn has finished yet");
        assert!(
            awaiting_prompt(Some(45), Some(&turn)),
            "the finished turn is older"
        );
        // A prompt steered into the running turn is answered by that turn,
        // which then carries the steered prompt's ordinal.
        turn.accepted_ordinal = Some(45);
        assert!(!awaiting_prompt(Some(45), Some(&turn)));
    }

    #[test]
    fn a_failed_turn_says_why_and_a_finished_one_did_not_fail() {
        assert_eq!(failed_turn(&finished("t", "end_turn"), Some("done")), None);
        assert_eq!(
            failed_turn(&finished("t", "awaiting_input"), Some("?")),
            None
        );
        let mut error = finished("t", "error");
        assert_eq!(
            failed_turn(&error, Some("You've hit your usage limit.")),
            Some(("failed", "You've hit your usage limit.".to_owned()))
        );
        error.diagnostic = Some(crate::diagnostic::TurnDiagnostic {
            message: "usageLimitExceeded".into(),
            code: None,
            http_status: None,
            reset_at: None,
        });
        assert_eq!(
            failed_turn(&error, Some("You've hit your usage limit.")),
            Some(("failed", "usageLimitExceeded".to_owned()))
        );
        assert_eq!(
            failed_turn(&finished("t", "cancelled"), None),
            Some(("interrupted", "the turn was cancelled".to_owned()))
        );
    }

    #[test]
    fn only_the_reminder_prefix_names_a_reminder() {
        assert!(is_handback_reminder("handback-reminder-0a1b"));
        assert!(!is_handback_reminder("handback-reminderx-0a1b"));
        assert!(!is_handback_reminder("api-0a1b"));
    }

    #[test]
    fn a_record_without_the_tool_flag_reads_as_having_no_tool() {
        let record: SubagentRecord = serde_json::from_str(
            r#"{"child_session_id":"c","parent_session_id":"p","task_name":"t","profile_id":"pr","working_directory":".","initial_prompt":"i","request_key":"k","created_at":"2026-09-15"}"#,
        )
        .expect("records written before the tool existed remain readable");
        assert!(!record.handback_tool);
        let encoded = serde_json::to_value(&record).expect("record encodes");
        assert!(encoded.get("handback_tool").is_none(), "{encoded}");
    }

    #[test]
    fn the_mcp_role_round_trips_through_its_argument() {
        for role in [SubagentMcpRole::Parent, SubagentMcpRole::Child] {
            assert_eq!(role.id().parse::<SubagentMcpRole>().unwrap(), role);
        }
        assert!("grandchild".parse::<SubagentMcpRole>().is_err());
    }

    #[test]
    fn noticed_turn_keeps_the_stored_delivered_turn_field_name() {
        let record: SubagentRecord = serde_json::from_str(
            r#"{"child_session_id":"c","parent_session_id":"p","task_name":"t","profile_id":"pr","working_directory":".","initial_prompt":"i","request_key":"k","created_at":"2026-09-15","delivered_turn":3}"#,
        )
        .expect("stored relation payloads remain readable");
        assert_eq!(record.noticed_turn, Some(3));
        let encoded = serde_json::to_value(&record).expect("record encodes");
        assert_eq!(encoded["delivered_turn"], 3);
        assert!(
            encoded.get("noticed_turn").is_none(),
            "the wire field name must stay historical: {encoded}"
        );
    }

    #[test]
    fn a_wait_timeout_is_clamped_into_the_advertised_range() {
        use std::time::Duration;
        assert_eq!(
            subagent_wait_timeout(None),
            Duration::from_secs(DEFAULT_WAIT_SECONDS)
        );
        assert_eq!(subagent_wait_timeout(Some(0)), Duration::from_secs(1));
        assert_eq!(
            subagent_wait_timeout(Some(1_700)),
            Duration::from_secs(1_700)
        );
        assert_eq!(
            subagent_wait_timeout(Some(MAX_WAIT_SECONDS * 2)),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
    }

    #[test]
    fn a_codex_parents_wait_fits_under_that_clients_own_three_hundred_second_limit() {
        use crate::config::HarnessKind;
        use std::time::Duration;
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Codex), Some(3_600)),
            Duration::from_secs(MAX_CODEX_WAIT_SECONDS)
        );
        // The default wait is longer than Codex allows, so it is capped too.
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Codex), None),
            Duration::from_secs(MAX_CODEX_WAIT_SECONDS)
        );
        // Claude's limit is on silence, which progress notifications break.
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Claude), Some(3_600)),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
        assert_eq!(
            subagent_wait_timeout_for(None, Some(3_600)),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
    }

    #[test]
    fn the_remaining_wait_counts_from_the_callers_request_and_survives_clock_skew() {
        use std::time::Duration;
        // Forty seconds of a forty-five second wait have already gone by.
        assert_eq!(
            remaining_subagent_wait(1_000_000, Some(45), 1_040_000),
            Duration::from_secs(5)
        );
        // A request whose deadline has passed answers at once.
        assert_eq!(
            remaining_subagent_wait(1_000_000, Some(45), 1_600_000),
            Duration::ZERO
        );
        // A worker clock ahead of the daemon's cannot extend the wait.
        assert_eq!(
            remaining_subagent_wait(2_000_000, Some(45), 1_000_000),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn a_wait_without_a_timeout_waits_as_long_as_the_harness_allows() {
        use crate::config::HarnessKind;
        use std::time::Duration;
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Claude), None),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
        assert_eq!(
            subagent_wait_timeout_for(None, None),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Codex), None),
            Duration::from_secs(MAX_CODEX_WAIT_SECONDS)
        );
    }

    #[test]
    fn a_wait_for_every_child_is_unchanged_on_the_wire() {
        let all = SubagentToolAction::WaitAgents {
            child_session_ids: vec!["c1".into()],
            timeout_seconds: Some(5),
            return_when: ReturnWhen::All,
        };
        let encoded = serde_json::to_value(&all).unwrap();
        assert!(encoded["params"].get("return_when").is_none(), "{encoded}");
        // A request written before the field existed reads as a wait for all.
        let old: SubagentToolAction = serde_json::from_str(
            r#"{"action":"wait_agents","params":{"child_session_ids":["c1"],"timeout_seconds":5}}"#,
        )
        .unwrap();
        assert_eq!(old, all);
        let any = SubagentToolAction::WaitAgents {
            child_session_ids: vec!["c1".into()],
            timeout_seconds: None,
            return_when: ReturnWhen::Any,
        };
        let encoded = serde_json::to_value(&any).unwrap();
        assert_eq!(encoded["params"]["return_when"], "any");
        assert_eq!(
            serde_json::from_value::<SubagentToolAction>(encoded).unwrap(),
            any
        );
    }

    #[test]
    fn return_when_decides_whether_a_wait_is_answered() {
        assert!(ReturnWhen::All.satisfied(&[true, true]));
        assert!(!ReturnWhen::All.satisfied(&[true, false]));
        assert!(ReturnWhen::Any.satisfied(&[false, true]));
        assert!(!ReturnWhen::Any.satisfied(&[false, false]));
    }

    #[test]
    fn next_action_names_only_the_children_left_to_wait_for() {
        let ids = |names: &[&str]| names.iter().map(|&n| n.to_owned()).collect::<Vec<_>>();
        let total = ids(&["c1", "c2", "c3"]);
        assert!(next_action(&total, &[]).starts_with("All children finished"));
        let none = next_action(&total, &total);
        assert!(none.contains("Call wait again with the same"), "{none}");
        let some = next_action(&total, &ids(&["c3"]));
        assert!(
            some.contains("2 of 3") && some.contains("c3") && !some.contains("c1"),
            "{some}"
        );
    }

    #[test]
    fn a_long_report_is_cut_on_a_character_boundary_and_says_how_to_get_the_rest() {
        let short = "done".to_owned();
        assert_eq!(bounded_report(&short), (short.clone(), false));
        let exact = "é".repeat(MAX_HANDBACK_CHARS);
        assert_eq!(bounded_report(&exact), (exact.clone(), false));
        let long = "é".repeat(MAX_HANDBACK_CHARS + 25);
        let (cut, truncated) = bounded_report(&long);
        assert!(truncated);
        assert!(cut.starts_with(&exact));
        assert!(cut.contains("[truncated: 25 more characters"), "{cut}");
        assert!(cut.contains("send_input"), "{cut}");
    }

    #[test]
    fn the_report_rules_state_the_enforced_cap_and_the_test_failure_fields() {
        let cap = format!(
            "{},{:03}",
            MAX_HANDBACK_CHARS / 1000,
            MAX_HANDBACK_CHARS % 1000
        );
        assert!(
            HANDBACK_REPORT_RULES.contains(&cap),
            "{HANDBACK_REPORT_RULES}"
        );
        for needed in [
            "name",
            "one-line reason",
            "path of its log",
            "report directory",
        ] {
            assert!(HANDBACK_REPORT_RULES.contains(needed), "{needed}");
        }
        let note = handback_prompt_note("/workspace/p/.mj-agents/c1");
        assert!(note.contains("/workspace/p/.mj-agents/c1"), "{note}");
        assert!(note.contains(HANDBACK_REPORT_RULES));
    }

    #[test]
    fn the_still_running_answer_names_the_children_and_tells_the_model_to_ask_again() {
        let payload = still_running_payload(
            &["child-1".to_owned(), "child-2".to_owned()],
            45,
            Some("Mjolnir was late"),
        );
        assert_eq!(payload["status"], WAIT_STATUS_STILL_RUNNING);
        assert_eq!(payload["waited_seconds"], 45);
        assert_eq!(payload["agents"][1]["child_session_id"], "child-2");
        assert_eq!(payload["agents"][1]["finished"], false);
        assert_eq!(payload["note"], "Mjolnir was late");
        let next = payload["next_action"].as_str().expect("next_action text");
        assert!(
            next.contains("Call wait again") && next.contains("not a failure"),
            "{next}"
        );
    }

    #[test]
    fn a_task_summary_is_one_line_of_the_parents_own_instructions() {
        let report_dir = "/workspace/p/.mj-agents/c1";
        let prompt = format!(
            "{}\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>\n\n--- source \"a.rs\", lines 1-2 (one-based, inclusive) ---\nfn a() {{}}",
            handback_prompt_note(report_dir)
        );
        assert_eq!(
            task_summary(&prompt, Some(report_dir)).as_deref(),
            Some("Fix the parser. Keep the tests green.")
        );
        // Without the recorded directory the note is still recognised.
        assert_eq!(
            task_summary(&prompt, None).as_deref(),
            Some("Fix the parser. Keep the tests green.")
        );
        // A child without the tool has no note before its instructions.
        assert_eq!(
            task_summary("Review the docs.", None).as_deref(),
            Some("Review the docs.")
        );
        assert_eq!(task_summary("  \n ", None), None);
        let long = "word ".repeat(100);
        let cut = task_summary(&long, None).unwrap();
        assert_eq!(cut.chars().count(), STOPPED_TASK_CHARS);
        assert!(cut.ends_with("word…"), "{cut}");
    }

    #[test]
    fn a_child_has_handed_back_only_when_its_final_report_answers_the_newest_prompt() {
        let task = finished("task", "end_turn");
        let delivered = SubagentReport {
            handback: handback("task"),
            ..SubagentReport::default()
        };
        assert!(has_handed_back(true, &delivered, false, Some(&task), 0));
        // Still working, never finished a turn, or given a newer prompt.
        assert!(!has_handed_back(true, &delivered, true, Some(&task), 0));
        assert!(!has_handed_back(true, &delivered, false, None, 0));
        let newer = SubagentReport {
            awaited_ordinal: Some(9),
            ..delivered.clone()
        };
        assert!(!has_handed_back(true, &newer, false, Some(&task), 0));
        // Owes a report and has not been reminded, or was reminded just now.
        assert!(!has_handed_back(
            true,
            &SubagentReport::default(),
            false,
            Some(&task),
            0
        ));
        let reminded = SubagentReport {
            reminder: reminder("task"),
            ..SubagentReport::default()
        };
        assert!(!has_handed_back(true, &reminded, false, Some(&task), 1_000));
        // Its last message is the report once the reminder went unanswered,
        // and always for a child without the tool.
        assert!(has_handed_back(
            true,
            &reminded,
            false,
            Some(&task),
            1_000 + HANDBACK_REMINDER_GRACE_MS
        ));
        assert!(has_handed_back(
            false,
            &SubagentReport::default(),
            false,
            Some(&task),
            0
        ));
        // A turn that failed or asked a question handed nothing back.
        for stop_reason in ["cancelled", "awaiting_input", "refusal"] {
            assert!(
                !has_handed_back(
                    false,
                    &SubagentReport::default(),
                    false,
                    Some(&finished("task", stop_reason)),
                    0
                ),
                "{stop_reason}"
            );
        }
    }
}
