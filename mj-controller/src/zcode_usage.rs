//! GLM Coding Plan quota reported by ZCode's native provider endpoint.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use mj_core::credentials::MAX_CREDENTIAL_BYTES;

const CODING_PLAN_PROVIDER: &str = "builtin:zai-coding-plan";
const QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZcodeUsageWindow {
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

pub async fn query(home: &Path) -> Result<Vec<ZcodeUsageWindow>> {
    let config = read_config(home).await?;
    let provider = config
        .get("provider")
        .and_then(|value| value.get(CODING_PLAN_PROVIDER))
        .context("ZCode Coding Plan provider is not configured")?;
    if provider.get("enabled").and_then(serde_json::Value::as_bool) != Some(true) {
        bail!("ZCode Coding Plan provider is disabled")
    }
    let options = provider
        .get("options")
        .context("ZCode Coding Plan provider options are missing")?;
    let api_key = options
        .get("apiKey")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("ZCode Coding Plan API key is missing")?;
    let international = options
        .get("baseURL")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|url| url.contains("api.z.ai"));
    let host = if international {
        "https://api.z.ai"
    } else {
        "https://open.bigmodel.cn"
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build ZCode quota client")?;
    let response = client
        .get(format!("{host}{QUOTA_PATH}"))
        .bearer_auth(api_key)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("ZCode quota request timed out")
            } else {
                anyhow::anyhow!("ZCode quota request failed")
            }
        })?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        bail!("ZCode Coding Plan login expired")
    }
    if !status.is_success() {
        bail!("ZCode quota request returned HTTP {status}")
    }
    let body = read_bounded(response).await?;
    let payload: serde_json::Value =
        serde_json::from_slice(&body).context("decode ZCode quota response")?;
    if payload.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        bail!("ZCode quota service rejected the request")
    }
    let limits: Vec<Limit> = serde_json::from_value(
        payload
            .get("data")
            .and_then(|value| value.get("limits"))
            .cloned()
            .unwrap_or_default(),
    )
    .context("decode ZCode quota limits")?;
    let windows = limits
        .into_iter()
        .filter_map(parse_limit)
        .collect::<Vec<_>>();
    if windows.is_empty() {
        bail!("ZCode quota response contained no inference windows")
    }
    Ok(windows)
}

async fn read_config(home: &Path) -> Result<serde_json::Value> {
    let path = home.join("v2/config.json");
    let file = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("read ZCode configuration {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_CREDENTIAL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        bail!("ZCode configuration is too large")
    }
    serde_json::from_slice(&bytes).context("decode ZCode configuration")
}

fn parse_limit(limit: Limit) -> Option<ZcodeUsageWindow> {
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
    Some(ZcodeUsageWindow {
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
        let chunk = chunk.context("read ZCode quota response")?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("ZCode quota response is too large")
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
    #[ignore = "requires MJ_ZCODE_TEST_HOME with an authenticated Coding Plan profile"]
    async fn live_coding_plan_quota_has_inference_windows() {
        let home = std::env::var_os("MJ_ZCODE_TEST_HOME")
            .map(std::path::PathBuf::from)
            .expect("set MJ_ZCODE_TEST_HOME to an authenticated .zcode directory");
        let windows = query(&home).await.unwrap();
        assert!(!windows.is_empty());
        assert!(windows.iter().all(|window| window.remaining_percent <= 100));
    }
}
