//! DeepSWE Pass@1/cost catalog used by the Thor/Loki/Eitri council.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use url::Url;

pub const DEFAULT_URL: &str = "https://deepswe.datacurve.ai/artifacts/v1.1/leaderboard-live.json";
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_ROWS: usize = 1_000;
const BUNDLED_SNAPSHOT: &str = include_str!("deepswe_snapshot.json");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Leaderboard {
    #[serde(default)]
    pub generated_at: Option<String>,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Row {
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub pass_at_1: f64,
    pub mean_cost_usd: f64,
}

pub fn default_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("mj")
        .join("deepswe-v1.1.json")
}

fn parse(body: &str) -> Result<Leaderboard> {
    let file: Leaderboard = serde_json::from_str(body).context("parse DeepSWE leaderboard")?;
    ensure!(!file.rows.is_empty(), "DeepSWE leaderboard has no rows");
    ensure!(
        file.rows.len() <= MAX_ROWS,
        "DeepSWE leaderboard has too many rows"
    );
    for row in &file.rows {
        ensure!(!row.model.trim().is_empty(), "DeepSWE row has empty model");
        ensure!(
            row.pass_at_1.is_finite() && (0.0..=1.0).contains(&row.pass_at_1),
            "DeepSWE row has invalid Pass@1"
        );
        ensure!(
            row.mean_cost_usd.is_finite() && row.mean_cost_usd >= 0.0,
            "DeepSWE row has invalid cost"
        );
    }
    Ok(file)
}

fn read_cache(path: &Path) -> Option<Leaderboard> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| parse(&body).ok())
}

fn cache_fresh(path: &Path, ttl: Duration) -> bool {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= ttl)
}

fn write_cache(path: &Path, file: &Leaderboard) {
    let result = (|| -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create DeepSWE cache dir {}", parent.display()))?;
        }
        let body = serde_json::to_vec(file).context("serialize DeepSWE cache")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .with_context(|| format!("write DeepSWE cache {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("replace DeepSWE cache {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!("write DeepSWE cache: {error:#}");
    }
}

async fn fetch(url: &str) -> Result<Leaderboard> {
    let safe_url = Url::parse(url).context("parse DeepSWE URL")?;
    ensure!(
        matches!(safe_url.scheme(), "http" | "https"),
        "DeepSWE URL must use http or https"
    );
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build DeepSWE HTTP client")?
        .get(safe_url.clone())
        .send()
        .await
        .with_context(|| format!("GET {safe_url}"))?;
    ensure!(
        response.status().is_success(),
        "GET {safe_url}: HTTP {}",
        response.status()
    );
    if let Some(length) = response.content_length() {
        ensure!(
            length as usize <= MAX_BODY_BYTES,
            "DeepSWE body is too large"
        );
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read DeepSWE body")?;
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_BODY_BYTES,
            "DeepSWE body exceeded size limit"
        );
        body.extend_from_slice(&chunk);
    }
    parse(std::str::from_utf8(&body).context("DeepSWE body is not UTF-8")?)
}

/// Fresh cache -> network -> stale cache -> bundled snapshot. Never fails.
pub async fn load(cache_path: &Path, ttl: Duration, url: &str) -> Leaderboard {
    if cache_fresh(cache_path, ttl)
        && let Some(file) = read_cache(cache_path)
    {
        return file;
    }
    match fetch(url).await {
        Ok(file) => {
            write_cache(cache_path, &file);
            file
        }
        Err(error) => {
            tracing::warn!("refresh DeepSWE leaderboard ({error:#}); using fallback");
            read_cache(cache_path)
                .unwrap_or_else(|| parse(BUNDLED_SNAPSHOT).expect("bundled DeepSWE snapshot"))
        }
    }
}

/// Compare only exact High rows for reasoning-capable models. A model with no
/// effort rows remains eligible on its best default row.
pub fn eligible_high(rows: &[Row]) -> Vec<Row> {
    let mut grouped: HashMap<&str, Vec<&Row>> = HashMap::new();
    for row in rows {
        grouped.entry(&row.model).or_default().push(row);
    }
    let mut eligible = Vec::new();
    for model_rows in grouped.into_values() {
        let has_effort = model_rows.iter().any(|row| {
            row.reasoning_effort
                .as_deref()
                .is_some_and(|effort| !effort.trim().is_empty())
        });
        let selected = if has_effort {
            model_rows.into_iter().find(|row| {
                row.reasoning_effort
                    .as_deref()
                    .is_some_and(|effort| effort.eq_ignore_ascii_case("high"))
            })
        } else {
            model_rows.into_iter().max_by(|a, b| compare_strength(a, b))
        };
        if let Some(row) = selected {
            eligible.push(row.clone());
        }
    }
    eligible.sort_by(|a, b| compare_strength(b, a));
    eligible
}

fn compare_strength(a: &Row, b: &Row) -> std::cmp::Ordering {
    a.pass_at_1
        .total_cmp(&b.pass_at_1)
        .then_with(|| b.mean_cost_usd.total_cmp(&a.mean_cost_usd))
        .then_with(|| b.model.cmp(&a.model))
}

pub fn pareto_frontier(rows: &[Row]) -> Vec<Row> {
    let mut frontier: Vec<Row> = rows
        .iter()
        .filter(|candidate| {
            !rows.iter().any(|other| {
                other.pass_at_1 >= candidate.pass_at_1
                    && other.mean_cost_usd <= candidate.mean_cost_usd
                    && (other.pass_at_1 > candidate.pass_at_1
                        || other.mean_cost_usd < candidate.mean_cost_usd)
            })
        })
        .cloned()
        .collect();
    frontier.sort_by(|a, b| a.pass_at_1.total_cmp(&b.pass_at_1));
    frontier
}

pub fn sonnet_anchor(rows: &[Row]) -> Option<&Row> {
    rows.iter()
        .filter(|row| row.model.to_ascii_lowercase().contains("sonnet"))
        .max_by(|a, b| compare_strength(a, b))
}

/// Choose the cheapest Pareto point that meets the Sonnet High quality
/// floor. If none does, retain the strongest point on the frontier.
pub fn eitri_frontier_choice(rows: &[Row], sonnet_pass_at_1: f64) -> Option<Row> {
    let frontier = pareto_frontier(rows);
    frontier
        .iter()
        .filter(|row| row.pass_at_1 >= sonnet_pass_at_1)
        .min_by(|a, b| {
            a.mean_cost_usd
                .total_cmp(&b.mean_cost_usd)
                .then_with(|| b.pass_at_1.total_cmp(&a.pass_at_1))
                .then_with(|| a.model.cmp(&b.model))
        })
        .cloned()
        .or_else(|| {
            frontier.into_iter().max_by(|a, b| {
                a.pass_at_1
                    .total_cmp(&b.pass_at_1)
                    .then_with(|| b.mean_cost_usd.total_cmp(&a.mean_cost_usd))
                    .then_with(|| b.model.cmp(&a.model))
            })
        })
}

pub fn model_provider(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gpt-") || lower.starts_with("o1-") || lower.starts_with("o3-") {
        "openai"
    } else if lower.starts_with("claude-") {
        "anthropic"
    } else if lower.starts_with("gemini-") || lower.starts_with("gemma-") {
        "google"
    } else if lower.starts_with("glm-") {
        "zhipuai"
    } else if lower.starts_with("kimi-") {
        "moonshotai"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, effort: Option<&str>, pass: f64, cost: f64) -> Row {
        Row {
            model: model.to_string(),
            reasoning_effort: effort.map(str::to_string),
            pass_at_1: pass,
            mean_cost_usd: cost,
        }
    }

    #[test]
    fn high_filter_does_not_interpolate_and_keeps_default_only_models() {
        let rows = vec![
            row("a", Some("max"), 0.9, 9.0),
            row("a", Some("high"), 0.7, 4.0),
            row("b", Some("xhigh"), 0.8, 5.0),
            row("c", None, 0.6, 2.0),
        ];
        let got = eligible_high(&rows);
        assert_eq!(
            got.iter().map(|r| r.model.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert_eq!(got[0].pass_at_1, 0.7);
    }

    #[test]
    fn pareto_and_sonnet_floor_choose_cheapest_qualified_builder() {
        let rows = vec![
            row("claude-sonnet-5", Some("high"), 0.48, 7.0),
            row("luna", Some("high"), 0.44, 0.7),
            row("terra", Some("high"), 0.54, 1.1),
            row("sol", Some("high"), 0.69, 3.4),
            row("dominated", Some("high"), 0.43, 2.0),
        ];
        assert_eq!(eitri_frontier_choice(&rows, 0.48).unwrap().model, "terra");
    }

    #[test]
    fn eitri_floor_falls_back_to_strongest_frontier_model() {
        let rows = vec![
            row("cheap", Some("high"), 0.40, 0.5),
            row("strong", Some("high"), 0.47, 1.5),
        ];
        assert_eq!(eitri_frontier_choice(&rows, 0.48).unwrap().model, "strong");
    }

    #[test]
    fn ranking_ties_prefer_lower_cost_then_model_id() {
        let rows = vec![
            row("z", Some("high"), 0.5, 2.0),
            row("b", Some("high"), 0.5, 1.0),
            row("a", Some("high"), 0.5, 1.0),
        ];
        let got = eligible_high(&rows);
        assert_eq!(
            got.iter().map(|row| row.model.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "z"]
        );
    }

    #[tokio::test]
    async fn stale_cache_is_used_when_refresh_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("leaderboard.json");
        let expected = Leaderboard {
            generated_at: Some("cached".to_string()),
            rows: vec![row("cached-model", Some("high"), 0.5, 1.0)],
        };
        std::fs::write(&cache, serde_json::to_vec(&expected).unwrap()).unwrap();
        let got = load(&cache, Duration::ZERO, "http://127.0.0.1:9/nope").await;
        assert_eq!(got.rows, expected.rows);
    }

    #[test]
    fn bundled_snapshot_is_valid() {
        let parsed = parse(BUNDLED_SNAPSHOT).unwrap();
        let eligible = eligible_high(&parsed.rows);
        assert_eq!(eligible[0].model, "gpt-5-6-sol");
        let anchor = sonnet_anchor(&eligible).expect("Sonnet anchor");
        assert_eq!(
            eitri_frontier_choice(&eligible, anchor.pass_at_1)
                .unwrap()
                .model,
            "gpt-5-6-terra"
        );
    }
}
