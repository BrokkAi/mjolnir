//! Muse's turn extras, reported inside the prompt response's usage metadata.
use mj_core::usage::{ProviderTurnUsage, TokenUsage};
use std::collections::BTreeMap;

/// Record the model-leg accounting Muse reports beside the standard counters.
/// The extras live on the `Usage` object's own `_meta`, not on the response
/// `_meta`. Anything missing or malformed is left out rather than invented, so
/// a partial report still contributes the fields it does carry.
pub fn attach_provider_details(
    mut usage: TokenUsage,
    usage_meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> TokenUsage {
    let Some(muse) = usage_meta.and_then(|meta| meta.get("muse")) else {
        return usage;
    };
    let model_calls = muse.get("modelCalls").and_then(|value| value.as_u64());
    let api_duration_ms = muse.get("apiDurationMs").and_then(|value| value.as_u64());
    let model_usage = muse
        .get("modelUsage")
        .and_then(|value| value.as_object())
        .map(|models| {
            models
                .iter()
                .filter_map(|(model, report)| {
                    Some((model.clone(), model_tokens(report, usage.scope)?))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    if model_calls.is_none() && api_duration_ms.is_none() && model_usage.is_empty() {
        return usage;
    }
    usage.provider_details = Some(Box::new(ProviderTurnUsage {
        model_calls,
        api_duration_ms,
        model_usage,
        ..Default::default()
    }));
    usage
}

/// One `modelUsage` row. The three counters are required; a row missing any of
/// them is dropped rather than reported with an invented zero.
fn model_tokens(
    report: &serde_json::Value,
    scope: mj_core::usage::UsageScope,
) -> Option<TokenUsage> {
    let count = |field: &str| report.get(field).and_then(|value| value.as_u64());
    Some(TokenUsage {
        provider_details: None,
        scope,
        total_tokens: count("totalTokens")?,
        input_tokens: count("inputTokens")?,
        output_tokens: count("outputTokens")?,
        thought_tokens: count("thoughtTokens"),
        cached_read_tokens: count("cachedReadTokens"),
        cached_write_tokens: count("cachedWriteTokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::HarnessKind;
    use serde_json::json;

    fn standard() -> TokenUsage {
        let mut report = agent_client_protocol::schema::v1::Usage::new(130, 100, 30);
        report.meta = Some(meta(json!({"mjolnir.dev/usage-scope": "turn"})));
        TokenUsage::from_acp(HarnessKind::Muse, report)
    }

    fn meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().expect("object fixture").clone()
    }

    #[test]
    fn a_partial_muse_report_keeps_the_fields_it_does_carry() {
        let extras = meta(json!({"muse": {"modelCalls": 2, "apiDurationMs": null}}));
        let details = attach_provider_details(standard(), Some(&extras))
            .provider_details
            .expect("provider details");
        assert_eq!(details.model_calls, Some(2));
        assert_eq!(details.api_duration_ms, None);
        assert!(details.model_usage.is_empty());
    }
}
