//! Durable server retry state. Legacy capacity names remain in stored and wire formats.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryAssessment {
    pub command_id: String,
    pub ordinal: u64,
    pub evidence: crate::activity::verdict::TurnEvidence,
}

/// Read-only compatibility with capacity completions written by older workers.
pub const CAPACITY_STOP_REASON: &str = "ModelCapacity";

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
            command_id: format!("{SERVER_RETRY_PREFIX}{ordinal}"),
            submitted: false,
        }
    }

    pub fn status(&self, now_ms: i64) -> String {
        let seconds = (self.retry_at_ms.saturating_sub(now_ms).max(0) as u64).div_ceil(1000);
        format!(
            "Provider unavailable · retrying in {}m{:02}s",
            seconds / 60,
            seconds % 60
        )
    }
}

/// Recognize historical stop reasons written by an older worker.
pub fn is_capacity_stop_reason(stop_reason: &str) -> bool {
    stop_reason.eq_ignore_ascii_case(CAPACITY_STOP_REASON)
}

pub fn is_capacity_retry_command(command_id: &str) -> bool {
    retried_turn(command_id).is_some()
}

const SERVER_RETRY_PREFIX: &str = "server-retry-";

/// The completion ordinal of the turn a worker retry command sends again.
///
/// The worker names each retry after the turn it retries: its id carries the
/// ordinal at which that turn completed. Older workers used the
/// `capacity-retry-` prefix. `None` when the command is not a retry.
pub fn retried_turn(command_id: &str) -> Option<u64> {
    let ordinal = command_id
        .strip_prefix("capacity-retry-")
        .or_else(|| command_id.strip_prefix(SERVER_RETRY_PREFIX))?;
    (!ordinal.is_empty() && ordinal.bytes().all(|b| b.is_ascii_digit()))
        .then(|| ordinal.parse().ok())
        .flatten()
}
