//! ZCode's turn extras, reported beside the standard usage on the prompt response.
use mj_core::usage::{ProviderTurnUsage, TokenUsage};

/// Record the adapter's model request count when it reports one. The standard
/// counters already arrive in `response.usage`; anything missing or malformed
/// here leaves `provider_details` unset rather than inventing a value.
pub fn attach_provider_details(
    mut usage: TokenUsage,
    response_meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> TokenUsage {
    let model_calls = response_meta
        .and_then(|meta| meta.get("zcode"))
        .and_then(|zcode| zcode.get("usage"))
        .and_then(|zcode_usage| zcode_usage.get("modelRequestCount"))
        .and_then(|count| count.as_u64());
    if let Some(model_calls) = model_calls {
        usage.provider_details = Some(Box::new(ProviderTurnUsage {
            model_calls: Some(model_calls),
            ..Default::default()
        }));
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::HarnessKind;
    use serde_json::json;

    fn standard() -> TokenUsage {
        TokenUsage::from_acp(
            HarnessKind::Zcode,
            agent_client_protocol::schema::v1::Usage::new(130, 100, 30),
        )
    }

    fn meta(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        value.as_object().expect("object fixture").clone()
    }

    #[test]
    fn zcode_model_request_count_becomes_model_calls_without_touching_counters() {
        let extras = meta(
            json!({"zcode":{"usage":{"source":"backend","modelRequestCount":3,
            "webFetchRequests":1,"webSearchRequests":2}}}),
        );
        let result = attach_provider_details(standard(), Some(&extras));
        let details = result.provider_details.as_ref().expect("provider details");
        assert_eq!(details.model_calls, Some(3));
        assert_eq!(details.cost, None);
        assert!(details.model_usage.is_empty());
        assert_eq!(
            (
                result.total_tokens,
                result.input_tokens,
                result.output_tokens
            ),
            (130, 100, 30)
        );
    }

    #[test]
    fn zcode_absent_or_malformed_model_request_count_leaves_details_unset() {
        assert_eq!(attach_provider_details(standard(), None), standard());
        for value in [
            json!({}),
            json!({"zcode": {}}),
            json!({"zcode": {"usage": {}}}),
            json!({"zcode": {"usage": {"modelRequestCount": -1}}}),
            json!({"zcode": {"usage": {"modelRequestCount": "3"}}}),
            json!({"zcode": {"usage": {"modelRequestCount": 2.5}}}),
            json!({"zcode": {"usage": {"modelRequestCount": null}}}),
        ] {
            let result = attach_provider_details(standard(), Some(&meta(value)));
            assert_eq!(result.provider_details, None);
            assert_eq!(result, standard());
        }
    }
}
