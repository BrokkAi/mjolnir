//! Durable contracts for Mjolnir-managed child-agent sessions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Longest time a sub-agent completion wait may remain pending.
pub const MAX_WAIT_SECONDS: u64 = 3_600;

/// How long a `wait` call blocks when the caller gives no timeout.
pub const DEFAULT_WAIT_SECONDS: u64 = 300;

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
        "next_action": next_action(false, child_session_ids.len(), child_session_ids.len()),
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
pub fn next_action(complete: bool, unfinished: usize, total: usize) -> String {
    if complete {
        return "All children finished. Their reports are in each agent's output field.".to_owned();
    }
    format!(
        "{unfinished} of {total} child sessions are still running; this is not a failure. \
         Call wait again with the same child_session_ids to keep waiting, \
         or do other work first and call wait later."
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

/// The sentence added to a child's first prompt when it has the tool.
pub const HANDBACK_PROMPT_NOTE: &str = "When you finish, call the mj-agents handback tool with your full report. The session that started you reads that report, not the rest of this conversation.";

/// How long a sent reminder may be neither queued, running nor finished before
/// the child's report stops waiting for it. Someone removed it from the queue,
/// or it never reached the child; either way it is not coming.
pub const HANDBACK_REMINDER_GRACE_MS: i64 = 30_000;

/// The longest report a child can hand back: the same limit as a prompt.
pub const MAX_HANDBACK_CHARS: usize = 65_536;

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
}
