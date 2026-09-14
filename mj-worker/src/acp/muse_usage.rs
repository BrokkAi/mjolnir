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
    use mj_core::usage::UsageScope;
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
    fn muse_turn_extras_become_provider_details_without_touching_counters() {
        let extras = meta(json!({
            "mjolnir.dev/usage-scope": "turn",
            "muse": {
                "modelCalls": 4,
                "apiDurationMs": 8200,
                "modelUsage": {
                    "gemini-3-pro": {"totalTokens": 100, "inputTokens": 80, "outputTokens": 20,
                                     "thoughtTokens": 5, "cachedReadTokens": 40},
                    "gemini-3-flash": {"totalTokens": 30, "inputTokens": 20, "outputTokens": 10},
                },
            },
        }));
        let result = attach_provider_details(standard(), Some(&extras));
        assert_eq!(result.scope, UsageScope::Turn);
        assert_eq!(
            (
                result.total_tokens,
                result.input_tokens,
                result.output_tokens
            ),
            (130, 100, 30)
        );
        let details = result.provider_details.as_ref().expect("provider details");
        assert_eq!(details.model_calls, Some(4));
        assert_eq!(details.api_duration_ms, Some(8200));
        assert_eq!(details.cost, None);
        let pro = &details.model_usage["gemini-3-pro"];
        assert_eq!(pro.scope, UsageScope::Turn);
        assert_eq!(
            (pro.total_tokens, pro.thought_tokens, pro.cached_read_tokens),
            (100, Some(5), Some(40))
        );
        let flash = &details.model_usage["gemini-3-flash"];
        assert_eq!((flash.total_tokens, flash.cached_read_tokens), (30, None));
    }

    #[test]
    fn muse_extras_that_are_absent_or_malformed_stay_out_of_provider_details() {
        assert_eq!(attach_provider_details(standard(), None), standard());
        for value in [
            json!({}),
            json!({"muse": {}}),
            json!({"muse": {"modelCalls": "4"}}),
            json!({"muse": {"modelCalls": -1, "apiDurationMs": 2.5}}),
            json!({"muse": {"modelUsage": []}}),
            json!({"muse": {"modelUsage": {"m": {"inputTokens": 80, "outputTokens": 20}}}}),
        ] {
            assert_eq!(
                attach_provider_details(standard(), Some(&meta(value.clone()))),
                standard(),
                "unusable extras must not create provider details: {value}"
            );
        }
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
