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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_details: Option<Box<ProviderTurnUsage>>,
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
        harness: crate::config::HarnessKind,
        usage: agent_client_protocol::schema::v1::Usage,
    ) -> Self {
        use crate::config::HarnessKind;
        // Explicit adapter metadata takes precedence over legacy defaults. In
        // particular, an incomplete Codex report must never enter turn totals.
        let declared_scope = usage.meta.as_ref().and_then(|meta| {
            meta.get("mjolnir.dev/usage-scope")
                .and_then(|value| value.as_str())
        });
        let scope = match (harness, declared_scope) {
            (HarnessKind::Codex, Some("turn")) => UsageScope::Turn,
            (HarnessKind::Codex, Some(_)) => UsageScope::Unspecified,
            (HarnessKind::Claude, _) => UsageScope::Turn,
            (HarnessKind::Codex, None) => UsageScope::LastRequest,
            _ => UsageScope::Unspecified,
        };
        Self {
            provider_details: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HarnessKind;
    use agent_client_protocol::schema::v1::Usage;

    #[test]
    fn codex_scope_distinguishes_complete_partial_and_legacy_reports() {
        let legacy = TokenUsage::from_acp(HarnessKind::Codex, Usage::new(100, 80, 20));
        assert_eq!(legacy.scope, UsageScope::LastRequest);
        for (declared, expected) in [
            ("turn", UsageScope::Turn),
            ("unspecified", UsageScope::Unspecified),
            ("future_scope", UsageScope::Unspecified),
        ] {
            let mut report = Usage::new(100, 80, 20);
            report.meta = Some(serde_json::Map::from_iter([(
                "mjolnir.dev/usage-scope".into(),
                serde_json::json!(declared),
            )]));
            let result = TokenUsage::from_acp(HarnessKind::Codex, report);
            assert_eq!(result.scope, expected);
            assert_eq!(result.total_tokens, 100);
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

/// Provider-reported accounting for this turn, separate from session cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTurnUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ProviderTurnCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_calls: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub model_usage: std::collections::BTreeMap<String, TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTurnCost {
    pub usd_ticks: u64,
    /// Exact decimal USD amount: 10^10 ticks per USD, without float rounding.
    pub usd: String,
    pub is_partial: bool,
}

impl ProviderTurnCost {
    pub fn from_usd_ticks(usd_ticks: u64, is_partial: bool) -> Self {
        Self {
            usd_ticks,
            usd: format!(
                "{}.{:010}",
                usd_ticks / 10_000_000_000,
                usd_ticks % 10_000_000_000
            ),
            is_partial,
        }
    }
}
