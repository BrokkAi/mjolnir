//! Durable contracts for Mjolnir-managed child-agent sessions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A session's delegation policy. Single-model selectors belong to the user,
/// never to the agent calling spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentPolicy {
    #[default]
    Native,
    AllModels,
    SingleModel {
        model: String,
        effort: Option<String>,
    },
    None,
}

impl SubagentPolicy {
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native)
    }

    /// Profile setup offers native delegation or one explicitly chosen model.
    /// Legacy session policies remain readable, and children still use None.
    pub fn validate_profile(&self, kind: crate::config::HarnessKind) -> anyhow::Result<()> {
        match self {
            Self::Native => Ok(()),
            Self::SingleModel { model, effort } => {
                anyhow::ensure!(
                    kind.supports_delegation_tools(),
                    "Mjolnir subagents require a Claude or Codex profile"
                );
                anyhow::ensure!(!model.trim().is_empty(), "select a subagent model");
                anyhow::ensure!(
                    effort.as_ref().is_none_or(|value| !value.trim().is_empty()),
                    "subagent effort cannot be empty"
                );
                Ok(())
            }
            Self::AllModels | Self::None => {
                anyhow::bail!("profile subagents must be native or single_model")
            }
        }
    }

    pub fn uses_mjolnir(&self) -> bool {
        matches!(self, Self::AllModels | Self::SingleModel { .. })
    }

    pub fn suppresses_native(&self) -> bool {
        !matches!(self, Self::Native)
    }

    pub fn parent_role(&self) -> Option<SubagentMcpRole> {
        match self {
            Self::AllModels => Some(SubagentMcpRole::Parent),
            Self::SingleModel { .. } => Some(SubagentMcpRole::FixedParent),
            _ => None,
        }
    }

    /// Children and unsupported parent harnesses cannot acquire delegation
    /// tools. Claude/Codex children still receive their separate handback tool.
    pub fn for_launch(&self, harness: crate::config::HarnessKind, is_child: bool) -> Self {
        if !harness.supports_delegation_tools() {
            Self::Native
        } else if is_child {
            Self::None
        } else {
            self.clone()
        }
    }
}

/// Upgrade compatibility for persisted records and internal handoff only.
/// Public creation APIs deserialize the policy directly and reject booleans.
pub fn deserialize_optional_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SubagentPolicy>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Legacy(bool),
        Policy(SubagentPolicy),
    }
    Ok(
        Option::<Stored>::deserialize(deserializer)?.map(|stored| match stored {
            Stored::Legacy(true) => SubagentPolicy::AllModels,
            Stored::Legacy(false) => SubagentPolicy::Native,
            Stored::Policy(policy) => policy,
        }),
    )
}

pub fn deserialize_launch_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SubagentPolicy, D::Error> {
    Ok(deserialize_optional_policy(deserializer)?.unwrap_or_default())
}

/// The refusal code for a single-model choice discovery does not offer. A
/// client picks its own remedy from it: the daemon's message names no
/// surface, and only the client knows whether the person has a Settings
/// screen or command-line flags to change the choice with.
pub const CHOICE_UNAVAILABLE_CODE: &str = "subagent_choice_unavailable";

/// How a choice is matched to a profile. Surface-neutral, so it can follow
/// any message from [`SubagentOptions::validate`].
const ELIGIBILITY_NOTE: &str = "Mjolnir chooses an eligible profile offering that selection by remaining quota. This session's own profile is always eligible.";

/// Choices before a parent session exists, using the same eligibility as spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentOptions {
    pub models: Vec<crate::acp::SessionConfigChoice>,
    pub efforts: Vec<crate::acp::SessionConfigChoice>,
    pub unavailable: Vec<String>,
}

impl SubagentOptions {
    pub fn validate(&self, policy: &SubagentPolicy) -> Result<(), String> {
        let SubagentPolicy::SingleModel { model, effort } = policy else {
            return Ok(());
        };
        if model.is_empty() {
            return Err(format!("Choose a subagent model. {ELIGIBILITY_NOTE}"));
        }
        if !self.models.iter().any(|choice| &choice.value == model) {
            return Err(format!(
                "Selected subagent model {model:?} is unavailable.{} {ELIGIBILITY_NOTE}",
                choice_list(" Available models", &self.models)
            ));
        }
        if self.efforts.is_empty() && effort.is_none() {
            return Ok(());
        }
        if effort
            .as_ref()
            .is_some_and(|effort| self.efforts.iter().any(|choice| &choice.value == effort))
        {
            return Ok(());
        }
        if self.efforts.is_empty() {
            return Err(format!(
                "Subagent model {model:?} offers no efforts; leave the effort unset."
            ));
        }
        Err(format!(
            "Select an available effort for subagent model {model:?}.{} {ELIGIBILITY_NOTE}",
            choice_list(" Available efforts", &self.efforts)
        ))
    }
}

/// "<label>: a, b." for a non-empty list of choices, otherwise nothing.
fn choice_list(label: &str, choices: &[crate::acp::SessionConfigChoice]) -> String {
    if choices.is_empty() {
        return String::new();
    }
    let values = choices
        .iter()
        .map(|choice| choice.value.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{label}: {values}.")
}

pub fn delegation_policy(limit: usize) -> String {
    include_str!("../assets/subagent-delegation.md").replace("$N", &limit.to_string())
}

/// Longest time a sub-agent completion wait may remain pending.
pub const MAX_WAIT_SECONDS: u64 = 3_600;

/// Default `wait` duration before applying the caller harness's ceiling.
pub const DEFAULT_WAIT_SECONDS: u64 = MAX_WAIT_SECONDS;

/// The `spawn` model value that means "the model the parent is running now".
pub const CURRENT_MODEL: &str = "current";

/// `status` when this wait returned one or more previously unreported finishes.
pub const WAIT_STATUS_REPORTED: &str = "reported";

/// `status` when the deadline arrived first. It is an answer, not a failure:
/// the children are still working and the caller collects them by calling
/// `wait` again.
pub const WAIT_STATUS_STILL_RUNNING: &str = "still_running";

/// `status` when this wait found no new reports and no unfinished children.
pub const WAIT_STATUS_NOTHING_TO_WAIT_FOR: &str = "nothing_to_wait_for";

/// Identity of one finish already returned to the parent by `wait`.
///
/// Turn spans distinguish repeated finishes of a child resumed with
/// `send_input`. Terminal failures without a turn span use their state,
/// detail, and the last completed turn ordinal. Session metadata such as a
/// title or updated timestamp does not change the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubagentFinishIdentity {
    Turn {
        start_position: u64,
        completed_ordinal: u64,
        state: String,
    },
    Terminal {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        /// The last completed turn separates terminal failures after distinct
        /// runs. Old records stored an `updated_at` string here; consume it as
        /// `None` so those records remain readable without treating renames as
        /// new finishes.
        #[serde(
            default,
            alias = "updated_at",
            deserialize_with = "deserialize_terminal_last_completed_ordinal",
            skip_serializing_if = "Option::is_none"
        )]
        last_completed_ordinal: Option<u64>,
    },
}

fn deserialize_terminal_last_completed_ordinal<'de, D>(
    deserializer: D,
) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| value.as_u64()))
}

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

/// How long one `wait` call blocks for this harness. The shim, worker, and
/// daemon use the same default and harness ceiling so they agree on the answer
/// deadline without carrying a timeout in the request.
pub fn subagent_wait_timeout_for(
    harness: Option<crate::config::HarnessKind>,
) -> std::time::Duration {
    std::time::Duration::from_secs(DEFAULT_WAIT_SECONDS.min(max_wait_seconds_for(harness)))
}

/// What is left of a `wait` call's harness budget, counted from when the caller
/// made the request rather than from when work on it started. A request that is
/// executed again after a daemon restart still answers at its original
/// deadline instead of starting the window over.
///
/// The two clocks involved can belong to different hosts, so the elapsed time
/// is clamped into `0..=budget`: skew cannot extend the harness window or make
/// the remaining duration negative.
pub fn remaining_subagent_wait(
    created_at_ms: i64,
    harness: Option<crate::config::HarnessKind>,
    now_ms: i64,
) -> std::time::Duration {
    let budget = subagent_wait_timeout_for(harness);
    let elapsed_ms = now_ms.saturating_sub(created_at_ms).max(0) as u64;
    budget.saturating_sub(std::time::Duration::from_millis(elapsed_ms))
}

/// The worker-side answer when the daemon does not answer a `wait` by its
/// deadline. The worker does not own the child list or their current state.
pub fn still_running_payload(waited_seconds: u64, note: Option<&str>) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "status": WAIT_STATUS_STILL_RUNNING,
        "waited_seconds": waited_seconds,
        "agents": [],
        "next_action": next_action(WAIT_STATUS_STILL_RUNNING, 0, 0),
    });
    if let Some(note) = note
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("note".into(), serde_json::Value::String(note.into()));
    }
    payload
}

/// The one sentence that tells the parent what to do with a wait answer.
/// Child selection is owned by the daemon, so this never names child IDs.
pub fn next_action(status: &str, new_reports: usize, unfinished: usize) -> String {
    match status {
        WAIT_STATUS_REPORTED if unfinished > 0 => format!(
            "Collected {new_reports} new report(s). {unfinished} child session(s) are still unfinished; call wait again later to collect reports that become ready."
        ),
        WAIT_STATUS_REPORTED => format!(
            "Collected {new_reports} new report(s). Call wait again after a child has more work if you need another report."
        ),
        WAIT_STATUS_NOTHING_TO_WAIT_FOR => {
            "There are no new reports or unfinished children. Call wait again after spawning a child or giving one more input.".to_owned()
        }
        _ => {
            "This harness's wait window ended before a new report was available. Call wait again later to collect reports that become ready.".to_owned()
        }
    }
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
    /// Stamped by the worker owner at durable admission, never trusted from MCP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_command_id: Option<String>,
    pub request_id: String,
    pub created_at_ms: i64,
    pub action: SubagentToolAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    SendMessage {
        child_session_id: String,
        message: String,
    },
    WaitAgents,
    /// An accepted request from an older parent worker. New MCP servers do
    /// not expose interrupt, and the daemon rejects this legacy action.
    #[serde(rename = "interrupt_agent")]
    LegacyInterruptAgent {
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

#[derive(Deserialize)]
#[serde(
    tag = "action",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum SubagentToolActionWire {
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
    SendMessage {
        child_session_id: String,
        message: String,
    },
    WaitAgents,
    #[serde(rename = "interrupt_agent")]
    LegacyInterruptAgent {
        child_session_id: String,
    },
    CloseAgent {
        child_session_id: String,
    },
    Handback {
        message: String,
    },
}

impl SubagentToolAction {
    /// Whether accepting this action may change a child session's lifecycle or
    /// queued work. In-place Move drains these actions before stopping the
    /// parent worker; observations can be interrupted with the old harness.
    pub const fn mutates_child_state(&self) -> bool {
        matches!(
            self,
            Self::Spawn { .. }
                | Self::SendInput { .. }
                | Self::SendMessage { .. }
                | Self::LegacyInterruptAgent { .. }
                | Self::CloseAgent { .. }
        )
    }
}

impl<'de> Deserialize<'de> for SubagentToolAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let mut value = serde_json::Value::deserialize(deserializer)?;
        if value.get("action").and_then(serde_json::Value::as_str) == Some("wait_agents") {
            if let Some(params) = value
                .get_mut("params")
                .and_then(serde_json::Value::as_object_mut)
            {
                // Durable worker queues can outlive the build that wrote them.
                // Ignore retired wait fields, but preserve errors for every
                // field that is not part of an old wait request.
                params.remove("child_session_ids");
                params.remove("return_when");
                params.remove("timeout_seconds");
            }
            if value.get("params").is_some_and(|params| {
                params.is_null() || params.as_object().is_some_and(|m| m.is_empty())
            }) {
                value
                    .as_object_mut()
                    .expect("wait action is a JSON object")
                    .remove("params");
            }
        }
        let wire =
            serde_json::from_value::<SubagentToolActionWire>(value).map_err(D::Error::custom)?;
        Ok(match wire {
            SubagentToolActionWire::ListProfiles => Self::ListProfiles,
            SubagentToolActionWire::Spawn {
                task_name,
                instructions,
                profile_id,
                model,
                effort,
                working_directory,
                context,
                files,
            } => Self::Spawn {
                task_name,
                instructions,
                profile_id,
                model,
                effort,
                working_directory,
                context,
                files,
            },
            SubagentToolActionWire::ListAgents => Self::ListAgents,
            SubagentToolActionWire::SendInput {
                child_session_id,
                message,
            } => Self::SendInput {
                child_session_id,
                message,
            },
            SubagentToolActionWire::SendMessage {
                child_session_id,
                message,
            } => Self::SendMessage {
                child_session_id,
                message,
            },
            SubagentToolActionWire::WaitAgents => Self::WaitAgents,
            SubagentToolActionWire::LegacyInterruptAgent { child_session_id } => {
                Self::LegacyInterruptAgent { child_session_id }
            }
            SubagentToolActionWire::CloseAgent { child_session_id } => {
                Self::CloseAgent { child_session_id }
            }
            SubagentToolActionWire::Handback { message } => Self::Handback { message },
        })
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
    /// The last child finish whose report was durably returned by a parent
    /// `wait`. This is part of record_json, so adding it needs no SQL migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_finish: Option<SubagentFinishIdentity>,
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
    FixedParent,
    Child,
}

impl SubagentMcpRole {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::FixedParent => "fixed_parent",
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
                "send_message",
                "wait",
                "close",
            ],
            Self::FixedParent => &[
                "spawn",
                "list_agents",
                "send_input",
                "send_message",
                "wait",
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
            "fixed_parent" => Ok(Self::FixedParent),
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
/// shows it. A turn can end before the task is done (a harness failure, a
/// question in plain text), so the reminder must not order a child that has
/// not finished to stop: that turned a failed first turn into an empty report
/// (issue 1217).
pub const HANDBACK_REMINDER_TEXT: &str = "[handback reminder] Your turn ended without delivering a report. If your assigned task is finished, call the mj-agents handback tool now with your full report, then stop. If it is not finished, continue working on it, and hand back when it is done.";

/// A child's delegated authority and reporting requirements. Shared
/// by the first-prompt note, the child's server instructions and the handback
/// tool description, so the three cannot drift apart.
pub const HANDBACK_REPORT_RULES: &str = "Your report is what the parent reads, and it is at most 4,000 characters: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Write anything longer (logs, tables, full findings) to files in your report directory and list their paths in the report; never put it in the report itself. Fix mechanical errors yourself (compile errors, lint and format findings, test failures your own change caused) and list each fix in one line of your report. Complete the assigned outcome within the parent's agreed design and constraints. Resolve routine implementation details and perform necessary supporting work, including edits and tests in files or artifacts not named explicitly. Named files are starting pointers unless explicitly declared an ownership boundary. Respect read-only assignments, explicit exclusions, and other agents' ownership; do not overwrite concurrent changes. Ask the parent before changing agreed behavior or design, introducing an unapproved public or persisted contract change, crossing an ownership boundary, or taking an action outside delegated authority. Changes already required by the assignment, including contract or epoch changes, need no duplicate approval. Report unrelated baseline failures and continue independent assigned work. When a parent decision blocks further progress, hand back the question with the evidence needed to decide. The parent owns consequential design decisions and final acceptance.";

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

/// The command id of the handback reminder for the turn that finished at
/// `completed_ordinal`. The id is deterministic so a reminder is sent at most
/// once per turn, and [`reminded_turn_ordinal`] reads it back.
#[must_use]
pub fn handback_reminder_command_id(completed_ordinal: u64) -> String {
    format!("{HANDBACK_REMINDER_PREFIX}-{completed_ordinal}")
}

/// The ordinal at which the reminded turn finished, when `command_id` names a
/// handback reminder.
#[must_use]
pub fn reminded_turn_ordinal(command_id: &str) -> Option<u64> {
    command_id
        .strip_prefix(HANDBACK_REMINDER_PREFIX)?
        .strip_prefix('-')?
        .parse()
        .ok()
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
    prompt_unanswered(awaited_ordinal, last_turn.and_then(answered_ordinal))
}

/// Whether the prompt accepted at `awaited_ordinal` is still unanswered by a
/// finished turn that answers prompts up to `answered_ordinal` (see
/// [`answered_ordinal`]). This is the one comparison every reader applies.
#[must_use]
pub fn prompt_unanswered(awaited_ordinal: Option<u64>, answered_ordinal: Option<u64>) -> bool {
    awaited_ordinal
        .is_some_and(|awaited| answered_ordinal.is_none_or(|answered| answered < awaited))
}

/// The newest prompt a finished turn answers, by acceptance ordinal.
///
/// A prompt's turn answers the prompt accepted at its own ordinal. A handback
/// reminder is Mjolnir's, not the parent's: the store gives its turn no
/// acceptance ordinal, and the report the child hands back in it answers the
/// turn it reminded about, so it answers every prompt accepted before that
/// turn finished. A prompt accepted after that runs after the reminder and is
/// still owed.
#[must_use]
pub fn answered_ordinal(turn: &crate::state::MaterializedTurnOutcome) -> Option<u64> {
    reminded_turn_ordinal(&turn.command_id).or(turn.accepted_ordinal)
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
        TurnOutcomeKind::Interrupted { message, .. } => ("interrupted", message.clone()),
        TurnOutcomeKind::Rejected { message, .. } => ("failed", message.clone()),
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

/// Whether a child's last finished turn failed because the provider refused
/// its profile's login: the turn's diagnostic says so (an error kind such as
/// Codex's `unauthorized`, or an auth failure sentence), or the turn failed
/// with such a sentence as its reason. A usage limit is never one.
///
/// Only the recorded failure is read, never the child's own messages: a child
/// may well be writing about authentication errors.
#[must_use]
pub fn turn_failed_on_login(turn: &crate::state::MaterializedTurnOutcome) -> bool {
    if turn
        .diagnostic
        .as_ref()
        .is_some_and(crate::credentials::turn_diagnostic_reports_auth_failure)
    {
        return true;
    }
    failed_turn(turn, None).is_some_and(|(state, reason)| {
        state == "failed" && crate::credentials::text_reports_auth_failure(&reason)
    })
}

/// What a parent is told about a profile whose login the provider refused.
#[must_use]
pub fn login_invalid_reason(profile_id: &str) -> String {
    format!("the login is no longer valid; run `mj login --profile {profile_id}` and spawn again")
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

/// A child whose parent remains live across an in-place harness swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InPlaceSubagent {
    pub child_session_id: String,
    pub task_name: String,
    pub state: InPlaceSubagentState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPlaceSubagentState {
    Running,
    Parked,
}

impl InPlaceSubagentState {
    const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Parked => "parked",
        }
    }
}

/// Reserved hidden-context tag for children retained across an in-place swap.
pub const IN_PLACE_SUBAGENTS_TAG: &str = "mj-in-place-subagents";

fn safe_subagent_task_name(task_name: &str) -> String {
    let single_line = task_name.split_whitespace().collect::<Vec<_>>().join(" ");
    let safe = single_line.replace('<', "‹").replace('>', "›");
    if safe.chars().count() <= STOPPED_TASK_CHARS {
        return safe;
    }
    let kept: String = safe.chars().take(STOPPED_TASK_CHARS - 1).collect();
    format!("{}…", kept.trim_end())
}

/// Hidden instructions for the first prompt after an in-place swap. The new
/// harness has no outstanding tool call stack from the old harness process.
#[must_use]
pub fn in_place_subagents_prompt_context(children: &[InPlaceSubagent]) -> Option<String> {
    if children.is_empty() {
        return None;
    }
    let mut lines = vec![
        format!("<{IN_PLACE_SUBAGENTS_TAG}>"),
        "These sub-agents remain attached to this session after its in-place harness swap:".into(),
    ];
    for child in children {
        lines.push(format!(
            "- {} (child_session_id {}; {})",
            safe_subagent_task_name(&child.task_name),
            child.child_session_id,
            child.state.label()
        ));
    }
    lines.push(
        "Any wait that was in progress during the swap was interrupted; reissue wait without arguments to collect reports from these children.".into(),
    );
    lines.push(
        "Before resending input, call list_agents and check pending_inputs so you do not send it twice.".into(),
    );
    lines.push(format!("</{IN_PLACE_SUBAGENTS_TAG}>"));
    Some(lines.join("\n"))
}

/// Conversation line that tells the person the same children are still live.
#[must_use]
pub fn in_place_subagents_notice(children: &[InPlaceSubagent]) -> Option<String> {
    if children.is_empty() {
        return None;
    }
    let listed = children
        .iter()
        .map(|child| {
            format!(
                "{} (child_session_id {}; {})",
                safe_subagent_task_name(&child.task_name),
                child.child_session_id,
                child.state.label()
            )
        })
        .collect::<Vec<_>>();
    Some(format!(
        "In-place harness swap kept these sub-agents attached: {}. Any wait in progress was interrupted; reissue wait without arguments to collect reports. Check list_agents for pending_inputs before resending input.",
        listed.join(", ")
    ))
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

/// The tag of the hidden block that tells a resumed parent's model which
/// sub-agents its suspend stopped. It is a reserved `mj-` block, so
/// [`crate::relay::strip_hidden_prompt_context`] removes it wherever a harness
/// copied the prompt into text a person sees.
pub const STOPPED_SUBAGENTS_TAG: &str = "mj-stopped-subagents";

/// The hidden context the first prompt after a parent resumes carries when
/// its suspend stopped sub-agents: one line per child, then what the model
/// should do about them. `None` when nothing was stopped.
#[must_use]
pub fn stopped_subagents_prompt_context(stopped: &[StoppedSubagent]) -> Option<String> {
    if stopped.is_empty() {
        return None;
    }
    let total = stopped.len();
    let not_handed_back = stopped.iter().filter(|child| !child.handed_back).count();
    let mut lines = vec![
        format!("<{STOPPED_SUBAGENTS_TAG}>"),
        format!(
            "Mjolnir stopped {} when this session was suspended:",
            crate::text::counted(total, "sub-agent", "sub-agents")
        ),
    ];
    for child in stopped {
        let task = child
            .task
            .as_deref()
            .map(|task| format!(", task: {task}"))
            .unwrap_or_default();
        let handed_back = if child.handed_back {
            "had handed back"
        } else {
            "had not handed back"
        };
        lines.push(format!(
            "- \"{}\" (child_session_id {}){task}; {handed_back}",
            child.title, child.child_session_id
        ));
    }
    lines.push(
        match (total, not_handed_back) {
            (1, 1) => "Its work was not handed back; spawn it again if you still need it.",
            (1, _) => "It had handed back its report before it was stopped.",
            (_, 0) => "Each had handed back its report before it was stopped.",
            (total, count) if total == count => {
                "Their work was not handed back; spawn them again if you still need it."
            }
            _ => {
                "The work of those that had not handed back was lost; spawn them again if you still need it."
            }
        }
        .to_owned(),
    );
    lines.push(
        "A stopped sub-agent no longer exists: wait, send_input, send_message and close cannot reach it."
            .to_owned(),
    );
    lines.push(format!("</{STOPPED_SUBAGENTS_TAG}>"));
    Some(lines.join("\n"))
}

/// The conversation line a person sees when a parent whose suspend stopped
/// sub-agents resumes. It names only the children that had not handed back,
/// since those lost work; the idle ones that had are counted. `None` when
/// nothing was stopped.
#[must_use]
pub fn stopped_subagents_notice(stopped: &[StoppedSubagent]) -> Option<String> {
    if stopped.is_empty() {
        return None;
    }
    let working = stopped
        .iter()
        .filter(|child| !child.handed_back)
        .map(|child| format!("\"{}\"", child.title))
        .collect::<Vec<_>>();
    let idle = stopped.len() - working.len();
    if working.is_empty() {
        return Some(format!(
            "Suspend stopped {}.",
            crate::text::counted(idle, "idle sub-agent", "idle sub-agents")
        ));
    }
    let mut notice = format!(
        "Suspend stopped {}: {}",
        crate::text::counted(working.len(), "working sub-agent", "working sub-agents"),
        working.join(", ")
    );
    if idle > 0 {
        notice.push_str(&format!(
            "; {} stopped too",
            crate::text::counted(idle, "idle sub-agent was", "idle sub-agents were")
        ));
    }
    notice.push('.');
    Some(notice)
}

/// What a person is told before a suspend stops sub-agents that have not
/// handed back their reports. `None` when every child had handed back: those
/// are stopped without a word, since their reports already reached the parent.
#[must_use]
pub fn suspend_warning(not_handed_back: usize) -> Option<String> {
    match not_handed_back {
        0 => None,
        1 => Some("1 sub-agent has not handed back; suspending stops it".to_owned()),
        count => Some(format!(
            "{count} sub-agents have not handed back; suspending stops them"
        )),
    }
}

/// The same warning naming the children by their listed titles, for a
/// person choosing whether to suspend: up to three by name, then how many
/// more. The API keeps [`suspend_warning`]'s count.
#[must_use]
pub fn suspend_warning_naming(titles: &[String]) -> Option<String> {
    match titles.len() {
        0 => None,
        1 => Some(format!(
            "Sub-agent {} has not handed back; suspending stops it",
            quoted_titles(titles)
        )),
        _ => Some(format!(
            "Sub-agents {} have not handed back; suspending stops them",
            quoted_titles(titles)
        )),
    }
}

/// Titles in quotes, in a sentence: `"a"`, `"a" and "b"`, `"a", "b" and
/// "c"`. Past three, the rest are counted: `"a", "b", "c" and 2 more`.
#[must_use]
pub fn quoted_titles(titles: &[String]) -> String {
    const NAMED: usize = 3;
    let mut named = titles
        .iter()
        .take(NAMED)
        .map(|title| format!("\"{title}\""))
        .collect::<Vec<_>>();
    let last = if titles.len() > NAMED {
        format!("{} more", titles.len() - NAMED)
    } else {
        match named.pop() {
            Some(last) => last,
            None => return String::new(),
        }
    };
    if named.is_empty() {
        last
    } else {
        format!("{} and {last}", named.join(", "))
    }
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

    #[test]
    fn only_child_mutations_hold_an_in_place_move_drain() {
        let mutations = [
            SubagentToolAction::Spawn {
                task_name: "task".into(),
                instructions: "work".into(),
                profile_id: None,
                model: None,
                effort: None,
                working_directory: PathBuf::new(),
                context: None,
                files: Vec::new(),
            },
            SubagentToolAction::SendInput {
                child_session_id: "child".into(),
                message: "continue".into(),
            },
            SubagentToolAction::SendMessage {
                child_session_id: "child".into(),
                message: "note".into(),
            },
            SubagentToolAction::LegacyInterruptAgent {
                child_session_id: "child".into(),
            },
            SubagentToolAction::CloseAgent {
                child_session_id: "child".into(),
            },
        ];
        assert!(
            mutations
                .iter()
                .all(SubagentToolAction::mutates_child_state)
        );
        for observation in [
            SubagentToolAction::ListProfiles,
            SubagentToolAction::ListAgents,
            SubagentToolAction::WaitAgents,
            SubagentToolAction::Handback {
                message: "done".into(),
            },
        ] {
            assert!(!observation.mutates_child_state());
        }
    }

    #[test]
    fn policies_preserve_legacy_records_but_public_policy_rejects_booleans() {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(
                default,
                alias = "mjolnir_subagents",
                deserialize_with = "deserialize_optional_policy"
            )]
            subagents: Option<SubagentPolicy>,
        }
        for (json, expected) in [
            (
                r#"{"mjolnir_subagents":true}"#,
                Some(SubagentPolicy::AllModels),
            ),
            (
                r#"{"mjolnir_subagents":false}"#,
                Some(SubagentPolicy::Native),
            ),
            ("{}", None),
        ] {
            assert_eq!(
                serde_json::from_str::<Stored>(json).unwrap().subagents,
                expected
            );
        }
        assert!(serde_json::from_str::<SubagentPolicy>("true").is_err());
        let fixed = SubagentPolicy::SingleModel {
            model: "chosen".into(),
            effort: Some("high".into()),
        };
        for harness in [
            crate::config::HarnessKind::Claude,
            crate::config::HarnessKind::Codex,
        ] {
            for policy in [
                SubagentPolicy::Native,
                SubagentPolicy::AllModels,
                fixed.clone(),
                SubagentPolicy::None,
            ] {
                assert_eq!(policy.for_launch(harness, true), SubagentPolicy::None);
                assert_eq!(policy.for_launch(harness, false), policy);
                assert_eq!(
                    serde_json::from_value::<SubagentPolicy>(
                        serde_json::to_value(&policy).unwrap()
                    )
                    .unwrap(),
                    policy
                );
            }
        }
    }

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
            reason: None,
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

    // Hard-won: a91cfd04: A live wait reported a child completed 45 ms before its first turn and hid failures.
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

    // Hard-won: a91cfd04: A child failing on a usage limit looked completed and its parent never learned why.
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

    /// #1160: a child whose profile could not sign in died on its first
    /// request, and its parent read the error as the child's report. The
    /// turns below are the shapes that failure takes.
    // Hard-won: 1b077959: A live Codex login refusal looked like task output and caused repeated failed spawns.
    #[test]
    fn a_turn_the_provider_refused_for_its_login_is_a_login_failure() {
        let diagnostic = |message: &str, code: Option<&str>| crate::diagnostic::TurnDiagnostic {
            message: message.into(),
            code: code.map(str::to_owned),
            http_status: None,
            reset_at: None,
        };
        let failed_with = |message: &str, code: Option<&str>| {
            let mut turn = finished("t", "error");
            turn.diagnostic = Some(diagnostic(message, code));
            turn
        };
        // Codex could not refresh its ChatGPT login (R14-1).
        assert!(turn_failed_on_login(&failed_with(
            "Your access token could not be refreshed. Please log out and sign in again.",
            Some("unauthorized"),
        )));
        // Codex sent an API key the backend rejected.
        assert!(turn_failed_on_login(&failed_with(
            "unexpected status 401 Unauthorized: Incorrect API key provided: sk-svca****fvMA.",
            Some("-32603"),
        )));
        // The error kind alone is enough.
        assert!(turn_failed_on_login(&failed_with(
            "Your session ended.",
            Some("unauthorized")
        )));

        // A usage limit, another failure, and a turn that finished are not.
        assert!(!turn_failed_on_login(&failed_with(
            "You've hit your usage limit.",
            Some("usageLimitExceeded"),
        )));
        assert!(!turn_failed_on_login(&failed_with(
            "stream disconnected before completion",
            Some("responseStreamDisconnected"),
        )));
        assert!(!turn_failed_on_login(&finished("t", "end_turn")));
        assert!(!turn_failed_on_login(&finished("t", "cancelled")));

        assert_eq!(
            login_invalid_reason("codex4"),
            "the login is no longer valid; run `mj login --profile codex4` and spawn again"
        );
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
    fn terminal_finish_identity_ignores_legacy_timestamp_and_tracks_completed_turns() {
        let old: SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind": "terminal",
            "state": "error",
            "detail": "startup failed",
            "updated_at": "2026-10-06T12:00:00Z"
        }))
        .unwrap();
        assert_eq!(
            old,
            SubagentFinishIdentity::Terminal {
                state: "error".into(),
                detail: Some("startup failed".into()),
                last_completed_ordinal: None,
            }
        );

        let before: SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind": "terminal",
            "state": "error",
            "detail": "startup failed",
            "last_completed_ordinal": 8
        }))
        .unwrap();
        let after: SubagentFinishIdentity = serde_json::from_value(serde_json::json!({
            "kind": "terminal",
            "state": "error",
            "detail": "startup failed",
            "last_completed_ordinal": 9
        }))
        .unwrap();
        assert_ne!(before, after);
        let encoded = serde_json::to_value(&old).unwrap();
        assert!(encoded.get("updated_at").is_none());
    }

    #[test]
    fn wait_uses_the_default_capped_by_its_harness() {
        use std::time::Duration;
        assert_eq!(
            subagent_wait_timeout_for(None),
            Duration::from_secs(DEFAULT_WAIT_SECONDS)
        );
        assert_eq!(
            subagent_wait_timeout_for(Some(crate::config::HarnessKind::Codex)),
            Duration::from_secs(MAX_CODEX_WAIT_SECONDS)
        );
    }

    // Hard-won: 605bd734: A Codex tools/call timed out at 303 seconds against its 300-second client limit.
    #[test]
    fn a_codex_parents_wait_fits_under_that_clients_own_three_hundred_second_limit() {
        use crate::config::HarnessKind;
        use std::time::Duration;
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Codex)),
            Duration::from_secs(MAX_CODEX_WAIT_SECONDS)
        );
        // Claude's limit is on silence, which progress notifications break.
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Claude)),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
        assert_eq!(
            subagent_wait_timeout_for(None),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
    }

    // Hard-won: 89c54ab1: A daemon restart restarted the full wait timeout and clock skew could extend it.
    #[test]
    fn the_remaining_wait_counts_from_the_callers_request_and_survives_clock_skew() {
        use std::time::Duration;
        // Forty seconds of the default window have already gone by.
        assert_eq!(
            remaining_subagent_wait(1_000_000, None, 1_040_000),
            Duration::from_secs(DEFAULT_WAIT_SECONDS - 40)
        );
        // A request whose deadline has passed answers at once.
        assert_eq!(
            remaining_subagent_wait(
                1_000_000,
                Some(crate::config::HarnessKind::Codex),
                1_300_000,
            ),
            Duration::ZERO
        );
        // A worker clock ahead of the daemon's cannot extend the wait.
        assert_eq!(
            remaining_subagent_wait(2_000_000, None, 1_000_000),
            Duration::from_secs(DEFAULT_WAIT_SECONDS)
        );
    }

    #[test]
    fn an_old_wait_replay_ignores_its_timeout_and_uses_the_default_from_creation() {
        use crate::config::HarnessKind;
        use std::time::Duration;
        let old: SubagentToolRequest = serde_json::from_str(
            r#"{"request_id":"old-wait","created_at_ms":1000000,"action":{"action":"wait_agents","params":{"timeout_seconds":5}}}"#,
        )
        .unwrap();
        assert_eq!(old.action, SubagentToolAction::WaitAgents);
        assert_eq!(
            remaining_subagent_wait(old.created_at_ms, None, 1_010_000),
            Duration::from_secs(DEFAULT_WAIT_SECONDS - 10)
        );
        assert_eq!(
            remaining_subagent_wait(old.created_at_ms, Some(HarnessKind::Codex), 1_010_000,),
            Duration::from_secs(MAX_CODEX_WAIT_SECONDS - 10)
        );
    }

    #[test]
    fn an_old_interrupt_request_remains_readable_but_is_not_an_offered_tool() {
        let old: SubagentToolRequest = serde_json::from_str(
            r#"{"request_id":"old-interrupt","created_at_ms":1,"action":{"action":"interrupt_agent","params":{"child_session_id":"child"}}}"#,
        )
        .unwrap();
        assert_eq!(
            old.action,
            SubagentToolAction::LegacyInterruptAgent {
                child_session_id: "child".into()
            }
        );
        assert!(
            SubagentMcpRole::Parent
                .tool_names()
                .contains(&"send_message")
        );
        assert!(!SubagentMcpRole::Parent.tool_names().contains(&"interrupt"));
    }

    #[test]
    fn wait_serializes_without_params_and_old_shapes_deserialize() {
        let wait = SubagentToolAction::WaitAgents;
        let encoded = serde_json::to_value(&wait).unwrap();
        assert_eq!(encoded, serde_json::json!({"action":"wait_agents"}));

        for old_shape in [
            r#"{"action":"wait_agents"}"#,
            r#"{"action":"wait_agents","params":{}}"#,
            r#"{"action":"wait_agents","params":{"child_session_ids":["c1"],"timeout_seconds":5,"return_when":"any"}}"#,
        ] {
            let decoded: SubagentToolAction = serde_json::from_str(old_shape).unwrap();
            assert_eq!(decoded, wait, "{old_shape}");
        }
    }

    #[test]
    fn a_wait_request_rejects_unknown_fields_after_retired_fields_are_stripped() {
        let error = serde_json::from_str::<SubagentToolAction>(
            r#"{"action":"wait_agents","params":{"unexpected":true}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("WaitAgents"), "{error}");
        let error = serde_json::from_str::<SubagentToolAction>(
            r#"{"action":"send_input","params":{"child_session_id":"c1","message":"x","unexpected":true}}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected"), "{error}");
    }

    #[test]
    fn wait_uses_the_harness_default_not_a_legacy_timeout() {
        use crate::config::HarnessKind;
        use std::time::Duration;
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Claude)),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
        assert_eq!(
            subagent_wait_timeout_for(None),
            Duration::from_secs(MAX_WAIT_SECONDS)
        );
        assert_eq!(
            subagent_wait_timeout_for(Some(HarnessKind::Codex)),
            Duration::from_secs(MAX_CODEX_WAIT_SECONDS)
        );
    }

    // Hard-won: cc7e6a2a: Real child reports of 27 to 62 KB were inlined into waits and triggered 21 repeated polls.
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

    // Hard-won: 89c54ab1: Timed-out waits were presented as failures instead of a still-running result with an ask-again action.
    #[test]
    fn the_worker_side_still_running_answer_does_not_guess_child_state() {
        let payload = still_running_payload(45, Some("Mjolnir was late"));
        assert_eq!(payload["status"], WAIT_STATUS_STILL_RUNNING);
        assert_eq!(payload["waited_seconds"], 45);
        assert_eq!(payload["agents"], serde_json::json!([]));
        assert_eq!(payload["note"], "Mjolnir was late");
        let next = payload["next_action"].as_str().expect("next_action text");
        assert!(
            next.contains("Call wait again") && !next.contains("child_session_ids"),
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
        // Persisted prompts from before #1174 still expose the parent's task
        // after the injected note changes in a newer build.
        let old_prompt = format!(
            "You are a Mjolnir sub-agent. Finish every task by calling the mj-agents handback tool with your report: the session that started you reads only that report, not the rest of this conversation. Your report directory is {report_dir}. Your report is what the parent reads, and it is at most 4,000 characters: outcome, changed files, evidence paths, mechanical fixes, what needs a decision, and failures. For each failing test give its name, a one-line reason and the path of its log. Write anything longer (logs, tables, full findings) to files in your report directory and list their paths in the report; never put it in the report itself. Fix mechanical errors yourself (compile errors, lint and format findings, test failures your own change caused) and list each fix in one line of your report. Hand back a question and stop when the fix needs a design choice, touches a file you were not given, changes a persisted identity, a public contract or an epoch, or the failure also occurs on the base commit.\n\nFix the parser.\nKeep the tests green.\n\n<parent_context>\nprivate\n</parent_context>"
        );
        for directory in [Some(report_dir), None] {
            assert_eq!(
                task_summary(&old_prompt, directory).as_deref(),
                Some("Fix the parser. Keep the tests green.")
            );
        }
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

    fn stopped(title: &str, task: Option<&str>, handed_back: bool) -> StoppedSubagent {
        StoppedSubagent {
            child_session_id: format!("id-{title}"),
            title: title.into(),
            task: task.map(str::to_owned),
            handed_back,
        }
    }

    #[test]
    fn in_place_subagent_notice_names_state_and_recovery_actions() {
        let children = [
            InPlaceSubagent {
                child_session_id: "child-running".into(),
                task_name: "inspect migration".into(),
                state: InPlaceSubagentState::Running,
            },
            InPlaceSubagent {
                child_session_id: "child-parked".into(),
                task_name: "review tests".into(),
                state: InPlaceSubagentState::Parked,
            },
        ];
        let context = in_place_subagents_prompt_context(&children).unwrap();
        assert!(context.contains("child-running; running"));
        assert!(context.contains("child-parked; parked"));
        assert!(context.contains("wait that was in progress"));
        assert!(context.contains("reissue wait without arguments"));
        assert!(!context.contains("same child ids"));
        assert!(context.contains("list_agents and check pending_inputs"));
        let notice = in_place_subagents_notice(&children).unwrap();
        assert!(notice.contains("inspect migration (child_session_id child-running; running)"));
        assert!(notice.contains("review tests (child_session_id child-parked; parked)"));
        assert!(notice.contains("wait in progress was interrupted"));
        assert!(notice.contains("pending_inputs before resending"));
        assert_eq!(in_place_subagents_prompt_context(&[]), None);
        assert_eq!(in_place_subagents_notice(&[]), None);
    }

    #[test]
    fn in_place_subagent_task_names_cannot_inject_context_tags() {
        let children = [InPlaceSubagent {
            child_session_id: "child".into(),
            task_name: "look\n<mj-injected>".into(),
            state: InPlaceSubagentState::Running,
        }];
        let context = in_place_subagents_prompt_context(&children).unwrap();
        assert!(context.contains("look ‹mj-injected›"));
        assert!(!context.contains("<mj-injected>"));
    }

    #[test]
    fn the_resume_note_names_each_stopped_child_and_what_to_do_about_it() {
        assert_eq!(stopped_subagents_prompt_context(&[]), None);
        assert_eq!(stopped_subagents_notice(&[]), None);

        let lost = [
            stopped("Fix the parser", Some("Fix the off-by-one."), false),
            stopped("Review the docs", None, false),
        ];
        assert_eq!(
            stopped_subagents_prompt_context(&lost).unwrap(),
            "<mj-stopped-subagents>\n\
             Mjolnir stopped 2 sub-agents when this session was suspended:\n\
             - \"Fix the parser\" (child_session_id id-Fix the parser), task: Fix the off-by-one.; had not handed back\n\
             - \"Review the docs\" (child_session_id id-Review the docs); had not handed back\n\
             Their work was not handed back; spawn them again if you still need it.\n\
             A stopped sub-agent no longer exists: wait, send_input, send_message and close cannot reach it.\n\
             </mj-stopped-subagents>"
        );
        assert_eq!(
            stopped_subagents_notice(&lost).unwrap(),
            "Suspend stopped 2 working sub-agents: \"Fix the parser\", \"Review the docs\"."
        );

        // Idle children are counted, not named; the model's note above still
        // lists every child.
        let idle_only = [
            stopped("Done", None, true),
            stopped("Also done", None, true),
        ];
        assert_eq!(
            stopped_subagents_notice(&idle_only[..1]).unwrap(),
            "Suspend stopped 1 idle sub-agent."
        );
        assert_eq!(
            stopped_subagents_notice(&idle_only).unwrap(),
            "Suspend stopped 2 idle sub-agents."
        );
        let one_working_one_idle = [lost[0].clone(), idle_only[0].clone()];
        assert_eq!(
            stopped_subagents_notice(&one_working_one_idle).unwrap(),
            "Suspend stopped 1 working sub-agent: \"Fix the parser\"; \
             1 idle sub-agent was stopped too."
        );
        let mixed_many = [
            lost[0].clone(),
            lost[1].clone(),
            idle_only[0].clone(),
            idle_only[1].clone(),
        ];
        assert_eq!(
            stopped_subagents_notice(&mixed_many).unwrap(),
            "Suspend stopped 2 working sub-agents: \"Fix the parser\", \"Review the docs\"; \
             2 idle sub-agents were stopped too."
        );

        let one = stopped_subagents_prompt_context(&lost[..1]).unwrap();
        assert!(
            one.contains("Mjolnir stopped 1 sub-agent when")
                && one.contains("Its work was not handed back; spawn it again"),
            "{one}"
        );
        let mixed = [lost[0].clone(), stopped("Done", None, true)];
        let note = stopped_subagents_prompt_context(&mixed).unwrap();
        assert!(note.contains("\"Done\" (child_session_id id-Done); had handed back"));
        assert!(note.contains("The work of those that had not handed back was lost"));
        let finished = stopped_subagents_prompt_context(&mixed[1..]).unwrap();
        assert!(
            finished.contains("It had handed back its report"),
            "{finished}"
        );
        // A reserved block, so it never shows in a harness's own copy of the prompt.
        assert_eq!(
            crate::relay::strip_hidden_prompt_context(&format!("{note}\nuser text")),
            "user text"
        );
    }

    // Hard-won: 1349c768: Suspend confirmation counted unfinished children but did not identify which children would stop.
    #[test]
    fn the_suspend_confirmation_names_up_to_three_children_and_counts_the_rest() {
        let titles = |count: usize| {
            ["Alpha", "Bravo", "Charlie", "Delta", "Echo"][..count]
                .iter()
                .map(|title| (*title).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(suspend_warning_naming(&titles(0)), None);
        assert_eq!(
            suspend_warning_naming(&titles(1)).as_deref(),
            Some("Sub-agent \"Alpha\" has not handed back; suspending stops it")
        );
        assert_eq!(
            suspend_warning_naming(&titles(2)).as_deref(),
            Some("Sub-agents \"Alpha\" and \"Bravo\" have not handed back; suspending stops them")
        );
        assert_eq!(
            suspend_warning_naming(&titles(3)).as_deref(),
            Some(
                "Sub-agents \"Alpha\", \"Bravo\" and \"Charlie\" have not handed back; \
                 suspending stops them"
            )
        );
        assert_eq!(
            suspend_warning_naming(&titles(4)).as_deref(),
            Some(
                "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 1 more have not handed \
                 back; suspending stops them"
            )
        );
        assert_eq!(
            suspend_warning_naming(&titles(5)).as_deref(),
            Some(
                "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 2 more have not handed \
                 back; suspending stops them"
            )
        );
    }

    /// A report handed back in a reminder turn answers the prompts accepted
    /// before the reminded turn finished, though the reminder turn has no
    /// acceptance ordinal of its own (I1-3, I1-4).
    // Hard-won: e1207055: Two live waits parked forever when the child's report arrived during a reminder turn.
    #[test]
    fn a_report_handed_back_in_a_reminder_turn_answers_the_reminded_prompt() {
        let reminder_id = handback_reminder_command_id(103);
        assert_eq!(reminded_turn_ordinal(&reminder_id), Some(103));
        assert_eq!(reminded_turn_ordinal("subagent-input-1"), None);
        let reminder_turn = crate::state::MaterializedTurnOutcome {
            accepted_ordinal: None,
            turn_start_position: None,
            completed_ordinal: 118,
            ..finished(&reminder_id, "end_turn")
        };
        let report = SubagentReport {
            handback: handback(&reminder_id),
            awaited_ordinal: Some(84),
            ..SubagentReport::default()
        };
        assert!(!awaiting_prompt(Some(84), Some(&reminder_turn)));
        assert!(has_handed_back(
            true,
            &report,
            false,
            Some(&reminder_turn),
            0
        ));
        // A prompt accepted after the reminded turn ended is still owed.
        assert!(awaiting_prompt(Some(110), Some(&reminder_turn)));
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
