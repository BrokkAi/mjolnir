//! Durable contracts for Mjolnir-managed child-agent sessions.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Longest time a sub-agent completion wait may remain pending.
pub const MAX_WAIT_SECONDS: u64 = 3_600;

/// Inclusive, one-based source lines captured for a child's initial context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRange {
    pub file: PathBuf,
    pub start: u64,
    pub end: u64,
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
        files: Vec<SourceRange>,
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
}
