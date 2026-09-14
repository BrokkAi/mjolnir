//! Grok's prompt ledger, reported in completion metadata and notifications.
use anyhow::{Context, Result, ensure};
use mj_core::usage::{ProviderTurnCost, ProviderTurnUsage, TokenUsage, UsageScope};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cached_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    model_calls: Option<u64>,
    api_duration_ms: Option<u64>,
    cost_usd_ticks: Option<u64>,
    #[serde(default)]
    cost_is_partial: bool,
    #[serde(default)]
    usage_is_incomplete: bool,
    #[serde(default)]
    model_usage: BTreeMap<String, serde_json::Value>,
}

pub(super) fn parse(value: &serde_json::Value, elapsed_ms: Option<u64>) -> Result<TokenUsage> {
    let report: Report = serde_json::from_value(value.clone()).context("decode Grok turn usage")?;
    normalize(report, elapsed_ms, true)
}

fn normalize(report: Report, elapsed_ms: Option<u64>, models: bool) -> Result<TokenUsage> {
    let total = report
        .input_tokens
        .checked_add(report.output_tokens)
        .context("Grok usage total overflow")?;
    ensure!(
        report.total_tokens.is_none_or(|n| n == total),
        "Grok usage total differs from full input plus output"
    );
    ensure!(
        report
            .reasoning_tokens
            .is_none_or(|n| n <= report.output_tokens),
        "Grok reasoning exceeds output tokens"
    );
    let cache = report
        .cached_read_tokens
        .unwrap_or(0)
        .checked_add(report.cache_creation_tokens.unwrap_or(0))
        .context("Grok cache token overflow")?;
    ensure!(
        cache <= report.input_tokens,
        "Grok cache exceeds full input tokens"
    );
    ensure!(
        models || report.model_usage.is_empty(),
        "nested Grok model usage is unsupported"
    );
    let mut model_usage = BTreeMap::new();
    for (name, value) in report.model_usage {
        let mut model: Report =
            serde_json::from_value(value).context("decode Grok per-model usage")?;
        model.usage_is_incomplete |= report.usage_is_incomplete;
        model.cost_is_partial |= report.cost_is_partial;
        model_usage.insert(name, normalize(model, None, false)?);
    }
    Ok(TokenUsage {
        scope: if report.usage_is_incomplete {
            UsageScope::Unspecified
        } else {
            UsageScope::Turn
        },
        input_tokens: report.input_tokens,
        output_tokens: report.output_tokens,
        total_tokens: total,
        thought_tokens: report.reasoning_tokens,
        cached_read_tokens: report.cached_read_tokens,
        cached_write_tokens: report.cache_creation_tokens,
        provider_details: Some(Box::new(ProviderTurnUsage {
            cost: report
                .cost_usd_ticks
                .map(|ticks| ProviderTurnCost::from_usd_ticks(ticks, report.cost_is_partial)),
            model_calls: report.model_calls,
            api_duration_ms: report.api_duration_ms,
            elapsed_ms,
            model_usage,
            credits: None,
        })),
    })
}

/// Only notifications observed during a live prompt are candidates. Correlate
/// their native prompt IDs with the response, never with a previous prompt.
#[derive(Default)]
pub(super) struct PendingUsage {
    session_id: Option<String>,
    reports: BTreeMap<String, Option<TokenUsage>>,
}

impl PendingUsage {
    pub fn begin(&mut self, session_id: String) {
        self.session_id = Some(session_id);
        self.reports.clear();
    }

    pub fn observe(&mut self, session_id: &str, update: &serde_json::Value) -> Result<bool> {
        if self.session_id.as_deref() != Some(session_id)
            || update.get("sessionUpdate").and_then(|v| v.as_str()) != Some("turn_completed")
        {
            return Ok(false);
        }
        let id = update
            .get("prompt_id")
            .and_then(|v| v.as_str())
            .context("Grok turn completion lacks prompt_id")?;
        let report = parse(
            update.get("usage").context("Grok completion lacks usage")?,
            update.get("elapsed_ms").and_then(|v| v.as_u64()),
        )?;
        ensure!(
            self.reports.contains_key(id) || self.reports.len() < 64,
            "too many unmatched Grok usage reports"
        );
        if let Some(previous) = self.reports.get_mut(id) {
            if previous.as_ref() != Some(&report) {
                *previous = None;
                anyhow::bail!("conflicting Grok usage reports for one prompt");
            }
        } else {
            self.reports.insert(id.into(), Some(report));
        }
        Ok(true)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.reports.contains_key(id)
    }

    pub fn finish(&mut self, id: Option<&str>) -> Option<TokenUsage> {
        self.session_id = None;
        let result = id.and_then(|id| self.reports.remove(id)).flatten();
        self.reports.clear();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage() -> serde_json::Value {
        json!({"inputTokens":100,"outputTokens":30,"totalTokens":130,"cachedReadTokens":40,
            "cacheCreationTokens":10,"reasoningTokens":20,"modelCalls":2,"apiDurationMs":1700,"costUsdTicks":88767200,
            "modelUsage":{"grok-4.6-build":{"inputTokens":100,"outputTokens":30,"cachedReadTokens":40,"reasoningTokens":20,"costUsdTicks":88767200}}})
    }

    #[test]
    fn grok_normalization_preserves_full_counts_exact_cost_and_model_keys() {
        let result = parse(&usage(), Some(2377)).unwrap();
        assert_eq!(
            (
                result.input_tokens,
                result.output_tokens,
                result.total_tokens
            ),
            (100, 30, 130)
        );
        assert_eq!(result.scope, UsageScope::Turn);
        let details = result.provider_details.unwrap();
        assert_eq!(details.cost.unwrap().usd, "0.0088767200");
        assert_eq!(details.elapsed_ms, Some(2377));
        assert_eq!(details.model_usage["grok-4.6-build"].input_tokens, 100);
    }

    #[test]
    fn grok_usage_keeps_zero_missing_and_incomplete_reports_distinct() {
        let minimal = parse(&json!({"inputTokens":0,"outputTokens":0}), None).unwrap();
        assert_eq!(minimal.total_tokens, 0);
        assert_eq!(minimal.cached_read_tokens, None);
        assert!(minimal.provider_details.unwrap().cost.is_none());
        let mut partial = usage();
        partial["usageIsIncomplete"] = json!(true);
        partial["costIsPartial"] = json!(true);
        let result = parse(&partial, None).unwrap();
        assert_eq!(result.scope, UsageScope::Unspecified);
        assert!(
            result
                .provider_details
                .as_ref()
                .unwrap()
                .cost
                .as_ref()
                .unwrap()
                .is_partial
        );
        assert_eq!(
            result.provider_details.unwrap().model_usage["grok-4.6-build"].scope,
            UsageScope::Unspecified
        );
        for malformed in [
            json!({"outputTokens":1}),
            json!({"inputTokens":-1,"outputTokens":2}),
            json!({"inputTokens":u64::MAX,"outputTokens":1}),
            json!({"inputTokens":1,"outputTokens":2,"totalTokens":4}),
        ] {
            assert!(parse(&malformed, None).is_err());
        }
    }

    #[test]
    fn grok_usage_is_correlated_and_duplicates_are_not_added() {
        let mut pending = PendingUsage::default();
        let update = json!({"sessionUpdate":"turn_completed","prompt_id":"p1","usage":usage()});
        assert!(!pending.observe("s", &update).unwrap());
        pending.begin("s".into());
        assert!(!pending.observe("foreign", &update).unwrap());
        assert!(pending.observe("s", &update).unwrap());
        assert!(pending.observe("s", &update).unwrap());
        assert_eq!(pending.finish(Some("p1")).unwrap().total_tokens, 130);
        pending.begin("s".into());
        pending.observe("s", &update).unwrap();
        assert!(pending.finish(Some("p2")).is_none());
        pending.begin("s".into());
        pending.observe("s", &update).unwrap();
        let mut conflicting = update;
        conflicting["usage"]["apiDurationMs"] = json!(9000);
        assert!(pending.observe("s", &conflicting).is_err());
        assert!(pending.finish(Some("p1")).is_none());
    }
}

#[derive(Clone, Default)]
pub(super) struct Collector {
    pending: std::sync::Arc<std::sync::Mutex<PendingUsage>>,
    changed: std::sync::Arc<tokio::sync::Notify>,
}

impl Collector {
    pub fn begin(&self, session_id: String) {
        self.pending
            .lock()
            .expect("Grok usage lock poisoned")
            .begin(session_id);
    }

    pub fn observe(&self, session_id: &str, update: &serde_json::Value) -> Result<()> {
        if self
            .pending
            .lock()
            .expect("Grok usage lock poisoned")
            .observe(session_id, update)?
        {
            self.changed.notify_one();
        }
        Ok(())
    }

    pub fn clear(&self) {
        self.pending
            .lock()
            .expect("Grok usage lock poisoned")
            .finish(None);
    }

    pub async fn complete(
        &self,
        meta: Option<&serde_json::Map<String, serde_json::Value>>,
        standard: Option<TokenUsage>,
    ) -> Result<Option<TokenUsage>> {
        let id = meta
            .and_then(|meta| meta.get("promptId").or_else(|| meta.get("requestId")))
            .and_then(|v| v.as_str());
        let reported = meta
            .and_then(|meta| meta.get("usage"))
            .map(|usage| parse(usage, None))
            .transpose();
        // Some versions send extension usage just after the ACP response. Bound
        // this wait; missing telemetry must never strand a completed prompt.
        if reported.as_ref().is_ok_and(|usage| usage.is_none())
            && let Some(id) = id
        {
            let wait = async {
                loop {
                    let notified = self.changed.notified();
                    if self
                        .pending
                        .lock()
                        .expect("Grok usage lock poisoned")
                        .contains(id)
                    {
                        break;
                    }
                    notified.await;
                }
            };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), wait).await;
        }
        let notification = self
            .pending
            .lock()
            .expect("Grok usage lock poisoned")
            .finish(id);
        match (reported?, notification) {
            (Some(mut response), Some(notification)) => {
                let elapsed = notification
                    .provider_details
                    .as_ref()
                    .and_then(|details| details.elapsed_ms);
                if let Some(details) = response.provider_details.as_mut() {
                    details.elapsed_ms = elapsed;
                }
                ensure!(
                    response == notification,
                    "Grok response and notification disagree about turn usage"
                );
                Ok(Some(response))
            }
            (Some(usage), None) | (None, Some(usage)) => Ok(Some(usage)),
            (None, None) => Ok(standard),
        }
    }
}
