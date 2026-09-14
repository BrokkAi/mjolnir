//! ZCode's turn extras, reported beside the standard usage on the prompt
//! response, plus the GLM Coding Plan credit delta measured around the turn.
use mj_core::usage::{ProviderTurnCredits, ProviderTurnUsage, TokenUsage};

/// Record the adapter's model request count and the measured credit delta. The
/// standard counters already arrive in `response.usage`; anything missing or
/// malformed here leaves `provider_details` unset rather than inventing a value.
pub fn attach_provider_details(
    mut usage: TokenUsage,
    response_meta: Option<&serde_json::Map<String, serde_json::Value>>,
    credits: Option<ProviderTurnCredits>,
) -> TokenUsage {
    let model_calls = response_meta
        .and_then(|meta| meta.get("zcode"))
        .and_then(|zcode| zcode.get("usage"))
        .and_then(|zcode_usage| zcode_usage.get("modelRequestCount"))
        .and_then(|count| count.as_u64());
    if model_calls.is_none() && credits.is_none() {
        return usage;
    }
    usage.provider_details = Some(Box::new(ProviderTurnUsage {
        model_calls,
        credits,
        ..Default::default()
    }));
    usage
}

/// GLM gives every non-token, non-MCP limit the same key (its lowercased type),
/// so both credit windows arrive as `credit_limit` and are told apart only by
/// their allowance.
const CREDIT_WINDOW_KEY: &str = "credit_limit";

/// One reading of the account's credit counter, taken from `account/usage_stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditReading {
    /// Window identity, stable across the two reads of one turn.
    pub window: String,
    /// The window's used counter (`currentValue` upstream).
    pub used: i64,
    /// When this window next resets, in epoch milliseconds.
    pub next_reset_ms: Option<i64>,
}

/// Pick the credit window out of an `account/usage_stats` response.
///
/// The GLM section reports two credit windows that share a counter: a 5-hour
/// one and a weekly one. The weekly window has the larger allowance and is far
/// less likely to reset mid-turn, so it wins; the 5-hour window is the fallback
/// when it is the only one present. Anything but a successful GLM section with
/// a usable counter returns `None`.
pub fn credit_reading(response: &serde_json::Value) -> Option<CreditReading> {
    let glm = response.get("glm")?;
    if glm.get("kind").and_then(|kind| kind.as_str()) != Some("success") {
        return None;
    }
    glm.get("items")?
        .as_array()?
        .iter()
        .filter(|item| {
            item.get("key")
                .and_then(|key| key.as_str())
                .is_some_and(|key| key.eq_ignore_ascii_case(CREDIT_WINDOW_KEY))
        })
        .filter_map(|item| {
            let used = number(item.get("usedCount"))?;
            let total = number(item.get("totalCount"));
            Some((
                total,
                CreditReading {
                    window: match total {
                        Some(total) => format!("{CREDIT_WINDOW_KEY}/{total}"),
                        None => CREDIT_WINDOW_KEY.to_owned(),
                    },
                    used,
                    next_reset_ms: number(item.get("nextResetTime")),
                },
            ))
        })
        .max_by_key(|(total, _)| *total)
        .map(|(_, reading)| reading)
}

/// JSON numbers arrive as floats from the adapter's counters; take whole values
/// only, so a surprising fractional counter is reported as unmeasured rather
/// than silently truncated.
fn number(value: Option<&serde_json::Value>) -> Option<i64> {
    let value = value?.as_f64()?;
    (value.fract() == 0.0 && value.abs() < 9e15).then_some(value as i64)
}

/// Turn two readings into the recorded delta. Readings of different windows
/// cannot be compared, so they measure nothing.
pub fn credit_delta(
    before: &CreditReading,
    after: &CreditReading,
    observed_at_ms: i64,
) -> Option<ProviderTurnCredits> {
    if before.window != after.window {
        return None;
    }
    Some(ProviderTurnCredits {
        used: after.used.saturating_sub(before.used),
        window: before.window.clone(),
        before: before.used,
        after: after.used,
        window_reset_crossed: before
            .next_reset_ms
            .is_some_and(|reset| reset <= observed_at_ms),
        account_shared: true,
    })
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

    fn credit_item(total: i64, used: i64, next_reset_ms: i64) -> serde_json::Value {
        json!({
            "key": "credit_limit", "label": "CREDIT_LIMIT",
            "usedPercent": 6, "leftPercent": 94,
            "usedCount": used, "totalCount": total, "nextResetTime": next_reset_ms,
        })
    }

    fn stats(items: serde_json::Value) -> serde_json::Value {
        json!({
            "glm": {"kind": "success", "level": "pro", "items": items},
            "opencode": {"kind": "not_configured"},
        })
    }

    #[test]
    fn zcode_model_request_count_becomes_model_calls_without_touching_counters() {
        let extras = meta(
            json!({"zcode":{"usage":{"source":"backend","modelRequestCount":3,
            "webFetchRequests":1,"webSearchRequests":2}}}),
        );
        let result = attach_provider_details(standard(), Some(&extras), None);
        let details = result.provider_details.as_ref().expect("provider details");
        assert_eq!(details.model_calls, Some(3));
        assert_eq!(details.cost, None);
        assert_eq!(details.credits, None);
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
        assert_eq!(attach_provider_details(standard(), None, None), standard());
        for value in [
            json!({}),
            json!({"zcode": {}}),
            json!({"zcode": {"usage": {}}}),
            json!({"zcode": {"usage": {"modelRequestCount": -1}}}),
            json!({"zcode": {"usage": {"modelRequestCount": "3"}}}),
            json!({"zcode": {"usage": {"modelRequestCount": 2.5}}}),
            json!({"zcode": {"usage": {"modelRequestCount": null}}}),
        ] {
            let result = attach_provider_details(standard(), Some(&meta(value)), None);
            assert_eq!(result.provider_details, None);
            assert_eq!(result, standard());
        }
    }

    #[test]
    fn credits_are_recorded_even_when_the_adapter_reports_no_model_calls() {
        let credits = credit_delta(
            &CreditReading {
                window: "credit_limit/140000".into(),
                used: 1874,
                next_reset_ms: Some(2_000),
            },
            &CreditReading {
                window: "credit_limit/140000".into(),
                used: 1931,
                next_reset_ms: Some(2_000),
            },
            1_500,
        )
        .expect("matching windows measure a delta");
        assert_eq!(
            (credits.used, credits.before, credits.after),
            (57, 1874, 1931)
        );
        assert!(!credits.window_reset_crossed);
        assert!(credits.account_shared);
        let details = attach_provider_details(standard(), None, Some(credits.clone()))
            .provider_details
            .expect("provider details");
        assert_eq!(details.model_calls, None);
        assert_eq!(details.credits, Some(credits));
    }

    #[test]
    fn the_weekly_credit_window_wins_over_the_five_hour_one() {
        let reading = credit_reading(&stats(json!([
            {"key": "token_5h", "label": "5h", "usedPercent": 10, "leftPercent": 90},
            credit_item(28_000, 1874, 5_000),
            credit_item(140_000, 1874, 900_000),
        ])))
        .expect("a credit window is present");
        assert_eq!(
            reading,
            CreditReading {
                window: "credit_limit/140000".into(),
                used: 1874,
                next_reset_ms: Some(900_000),
            }
        );
    }

    #[test]
    fn only_a_successful_glm_section_with_a_credit_counter_measures_anything() {
        for response in [
            json!({"glm": {"kind": "auth_error"}, "opencode": {"kind": "not_configured"}}),
            json!({"glm": {"kind": "rate_limited"}, "opencode": {"kind": "not_configured"}}),
            json!({"opencode": {"kind": "not_configured"}}),
            stats(json!([])),
            // Only token and MCP windows: no credit counter to read.
            stats(json!([
                {"key": "token_5h", "label": "5h", "usedPercent": 10, "leftPercent": 90},
                {"key": "mcp", "label": "MCP", "usedPercent": 1, "leftPercent": 99,
                 "usedCount": 3, "totalCount": 300},
            ])),
            // A credit window with no absolute counter cannot be differenced.
            stats(json!([
                {"key": "credit_limit", "label": "CREDIT_LIMIT",
                 "usedPercent": 6, "leftPercent": 94},
            ])),
        ] {
            assert_eq!(
                credit_reading(&response),
                None,
                "unusable quota response must measure nothing: {response}"
            );
        }
    }

    #[test]
    fn a_window_that_reset_between_the_reads_is_flagged_and_still_recorded() {
        let before = CreditReading {
            window: "credit_limit/140000".into(),
            used: 139_900,
            next_reset_ms: Some(1_000),
        };
        let after = CreditReading {
            window: "credit_limit/140000".into(),
            used: 40,
            next_reset_ms: Some(605_800_000),
        };
        let credits = credit_delta(&before, &after, 1_200).expect("still recorded");
        assert!(credits.window_reset_crossed);
        assert_eq!(credits.used, -139_860);
        assert_eq!((credits.before, credits.after), (139_900, 40));
    }

    #[test]
    fn readings_of_different_windows_measure_nothing() {
        let before = CreditReading {
            window: "credit_limit/140000".into(),
            used: 100,
            next_reset_ms: None,
        };
        let after = CreditReading {
            window: "credit_limit/28000".into(),
            used: 120,
            next_reset_ms: None,
        };
        assert_eq!(credit_delta(&before, &after, 0), None);
    }
}
