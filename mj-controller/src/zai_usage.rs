//! GLM Coding Plan quota, read from the Z.ai provider's monitor endpoint.
//!
//! A Codex profile that names Z.ai as its model provider authenticates with a
//! long-lived Coding Plan key from the profile's `environment`. The same key is
//! a bearer token for the provider's quota endpoint, so quota reporting needs
//! only the provider's host and that key.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use mj_core::codex_provider::CodexProviderKind;
use serde::Deserialize;

const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Whether `host` serves the Coding Plan quota endpoint. Only the Z.ai family
/// of hosts does, so the answer follows the provider kind.
pub fn serves_quota(host: &str) -> bool {
    CodexProviderKind::from_host(host) == CodexProviderKind::Zai
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZaiUsageWindow {
    pub label: String,
    pub remaining_percent: u8,
    pub used: Option<i64>,
    pub limit: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limit {
    #[serde(rename = "type")]
    kind: String,
    number: Option<u64>,
    usage: Option<i64>,
    current_value: Option<i64>,
    remaining: Option<i64>,
    percentage: Option<f64>,
    next_reset_time: Option<i64>,
}

/// Ask `host` (a bare host name such as `api.z.ai`) for the Coding Plan windows
/// belonging to `api_key`.
pub async fn query(host: &str, api_key: &str) -> Result<Vec<ZaiUsageWindow>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build Coding Plan quota client")?;
    let response = client
        .get(format!("https://{host}{QUOTA_PATH}"))
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("Coding Plan quota request timed out")
            } else {
                anyhow::anyhow!("Coding Plan quota request failed")
            }
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        bail!("Coding Plan API key was rejected")
    }
    if !status.is_success() {
        bail!("Coding Plan quota request returned HTTP {status}")
    }
    let body = read_bounded(response).await?;
    let payload: serde_json::Value =
        serde_json::from_slice(&body).context("decode Coding Plan quota response")?;
    if payload.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        bail!("Coding Plan quota service rejected the request")
    }
    let limits: Vec<Limit> = serde_json::from_value(
        payload
            .get("data")
            .and_then(|value| value.get("limits"))
            .cloned()
            .unwrap_or_default(),
    )
    .context("decode Coding Plan quota limits")?;
    let windows = limits
        .into_iter()
        .filter_map(parse_limit)
        .collect::<Vec<_>>();
    if windows.is_empty() {
        bail!("Coding Plan quota response contained no inference windows")
    }
    Ok(windows)
}

fn parse_limit(limit: Limit) -> Option<ZaiUsageWindow> {
    if limit.kind != "CREDIT_LIMIT" && limit.kind != "TOKENS_LIMIT" {
        return None;
    }
    let (used, total, remaining_percent) = match (limit.current_value, limit.remaining) {
        (Some(used), Some(remaining))
            if used >= 0 && remaining >= 0 && used.saturating_add(remaining) > 0 =>
        {
            let total = used.saturating_add(remaining);
            let percent = ((remaining as f64 / total as f64) * 100.0).round() as u8;
            (Some(used), Some(total), percent.min(100))
        }
        (Some(used), Some(_)) if used >= 0 && limit.usage.is_some_and(|total| total > 0) => {
            let total = limit.usage.unwrap();
            let percent = (100.0 - used as f64 / total as f64 * 100.0).round() as u8;
            (Some(used), Some(total), percent.min(100))
        }
        _ => {
            let used_percent = limit.percentage?.clamp(0.0, 100.0);
            (
                limit.current_value,
                limit.usage,
                (100.0 - used_percent).round() as u8,
            )
        }
    };
    let label = match limit.number {
        Some(5) => "5H".to_owned(),
        Some(1 | 7) => "Week".to_owned(),
        Some(number) => format!("{number}"),
        None => "Credits".to_owned(),
    };
    Some(ZaiUsageWindow {
        label,
        remaining_percent,
        used,
        limit: total,
        resets_at: limit
            .next_reset_time
            .map(|milliseconds| milliseconds / 1000),
    })
}

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read Coding Plan quota response")?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("Coding Plan quota response is too large")
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_observed_credit_windows_and_ignores_auxiliary_limits() {
        let five_hour = parse_limit(Limit {
            kind: "CREDIT_LIMIT".into(),
            number: Some(5),
            usage: Some(12_000),
            current_value: Some(300),
            remaining: Some(11_700),
            percentage: Some(3.0),
            next_reset_time: Some(1_789_370_152_403),
        })
        .unwrap();
        assert_eq!(five_hour.label, "5H");
        assert_eq!(five_hour.remaining_percent, 98);
        assert_eq!(five_hour.resets_at, Some(1_789_370_152));
        assert!(
            parse_limit(Limit {
                kind: "MCP_LIMIT".into(),
                number: None,
                usage: Some(10),
                current_value: Some(1),
                remaining: Some(9),
                percentage: None,
                next_reset_time: None,
            })
            .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires MJ_ZAI_TEST_KEY with a live Coding Plan key"]
    async fn live_coding_plan_quota_has_inference_windows() {
        let key =
            std::env::var("MJ_ZAI_TEST_KEY").expect("set MJ_ZAI_TEST_KEY to a Coding Plan API key");
        let windows = query("api.z.ai", &key).await.unwrap();
        assert!(!windows.is_empty());
        assert!(windows.iter().all(|window| window.remaining_percent <= 100));
    }

    #[test]
    fn only_the_coding_plan_hosts_serve_quota() {
        assert!(serves_quota("api.z.ai"));
        assert!(serves_quota("open.bigmodel.cn"));
        assert!(!serves_quota("api.openai.com"));
    }
}
