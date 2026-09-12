//! Capacity recovery is a live worker policy; historical text never schedules work.
use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use serde::{Deserialize, Serialize};

/// A classified live completion, recorded through the existing extensible stop reason.
pub const CAPACITY_STOP_REASON: &str = "ModelCapacity";
pub const CAPACITY_MESSAGE: &str = "Selected model is at capacity. Please try a different model.";

pub fn capacity_error(error: &agent_client_protocol::Error) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("codex_error_info"))
        .and_then(serde_json::Value::as_str)
        == Some("server_overloaded")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityRetry {
    pub attempt: u32,
    pub retry_at_ms: i64,
    pub command_id: String,
    /// Retained internally across the retry turn to advance its backoff.
    #[serde(default)]
    pub submitted: bool,
}

impl CapacityRetry {
    pub fn new(attempt: u32, ordinal: u64, now_ms: i64) -> Self {
        let delay_ms = 60_000_i64 * (1_i64 << attempt.saturating_sub(1).min(4));
        Self {
            attempt,
            retry_at_ms: now_ms.saturating_add(delay_ms),
            command_id: format!("capacity-retry-{ordinal}"),
            submitted: false,
        }
    }

    pub fn status(&self, now_ms: i64) -> String {
        let seconds = (self.retry_at_ms.saturating_sub(now_ms).max(0) as u64).div_ceil(1000);
        format!(
            "Model at capacity · retrying in {}m{:02}s",
            seconds / 60,
            seconds % 60
        )
    }
}

/// Whether a prompt stop reason means the model was at capacity and the
/// worker will retry on its own. Comparison is case-insensitive because stop
/// reasons are free text from the harness.
pub fn is_capacity_stop_reason(stop_reason: &str) -> bool {
    stop_reason.eq_ignore_ascii_case(CAPACITY_STOP_REASON)
}

pub fn is_capacity_retry_command(command_id: &str) -> bool {
    command_id
        .strip_prefix("capacity-retry-")
        .is_some_and(|ordinal| !ordinal.is_empty() && ordinal.bytes().all(|b| b.is_ascii_digit()))
}

/// Bounded final-message assembly. Tool/reasoning output separates messages,
/// and message IDs separate consecutive commentary and final replies.
#[derive(Default)]
pub struct CapacityResponse {
    message_id: Option<String>,
    text: String,
    overflow: bool,
}

impl CapacityResponse {
    pub fn observe(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let id = chunk.message_id.as_ref().map(ToString::to_string);
                if id != self.message_id {
                    *self = Self::default();
                    self.message_id = id;
                }
                if let ContentBlock::Text(content) = &chunk.content {
                    if self.text.len().saturating_add(content.text.len()) <= 512 {
                        self.text.push_str(&content.text);
                    } else {
                        self.overflow = true;
                    }
                } else {
                    self.overflow = true;
                }
            }
            SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::ToolCallUpdate(_) => *self = Self::default(),
            _ => {}
        }
    }

    pub fn at_capacity(&self) -> bool {
        !self.overflow && self.text.trim() == CAPACITY_MESSAGE
    }
}
