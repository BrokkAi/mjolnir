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

/// How long one `wait` call blocks, from what the caller asked for. The shim,
/// the worker and the daemon all resolve the caller's request through this one
/// function so the three cannot disagree about when the answer is due.
pub fn subagent_wait_timeout(requested: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_secs(
        requested
            .unwrap_or(DEFAULT_WAIT_SECONDS)
            .clamp(1, MAX_WAIT_SECONDS),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

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
