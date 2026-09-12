//! Provider-reported consumption. Context occupancy is deliberately excluded.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    Turn,
    LastRequest,
    Unspecified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub scope: UsageScope,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_write_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn from_acp(
        harness: crate::hel_config::HarnessKind,
        usage: agent_client_protocol::schema::v1::Usage,
    ) -> Self {
        use crate::hel_config::HarnessKind;
        // Verified against the managed Claude 0.73.0 and Codex 1.11.1 adapters.
        // Recheck these semantics when changing harness pins. Other adapters
        // are preserved without guessing whether they report cumulative counts.
        let scope = match harness {
            HarnessKind::Claude => UsageScope::Turn,
            HarnessKind::Codex => UsageScope::LastRequest,
            _ => UsageScope::Unspecified,
        };
        Self {
            scope,
            total_tokens: usage.total_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            thought_tokens: usage.thought_tokens,
            cached_read_tokens: usage.cached_read_tokens,
            cached_write_tokens: usage.cached_write_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderCost {
    /// Cumulative amount reported for the provider's session, not a turn delta.
    pub amount: f64,
    pub currency: String,
    pub observed_at_ms: i64,
}
