//! Model strength scores (LMArena Elo) — fetch + cache + process-global store.
//!
//! Mirrors [`crate::registry`]: a `load_*` function refreshes the leaderboard
//! from a URL, caches a normalized copy under `~/.cache/mj/scores.json`, and
//! refreshes only when the cached copy is older than [`CACHE_TTL`].
//! Offline-friendly with a belt-and-braces fallback chain: fresh cache → network
//! → stale cache → **bundled snapshot** compiled into the binary, so the picker
//! always has *something* to show.
//!
//! The runtime source is the **official LMArena leaderboard dataset** on
//! HuggingFace ([`DEFAULT_SCORES_URL`]) — a parquet file fetched over plain
//! HTTPS, no auth, redistributable. It is parsed to our small JSON schema and
//! that is what gets cached.
//!
//! Scores are joined onto an agent's selectable models via [`crate::model_resolve`]:
//! each leaderboard row and each model option are reduced to the same normalized
//! match key. No key match → no score (the picker renders `—`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use arrow_array::{Array, Float64Array, RecordBatch, StringArray};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::model_resolve::{self, MatchKey, MatchSpecificity};

/// How long a cached scores copy is considered fresh. New frontier models reach
/// LMArena within ~24–48h; weekly polling is ample for "new models show up".
pub const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Official LMArena leaderboard dataset (HuggingFace), `text` subset / `latest`
/// split — the overall Chatbot Arena board. Parquet over HTTPS, no auth,
/// redistributable. Overridable via config (`[scores] url = …`).
pub const DEFAULT_SCORES_URL: &str = "https://huggingface.co/datasets/lmarena-ai/leaderboard-dataset/resolve/main/text/latest-00000-of-00001.parquet";

/// The leaderboard category we score against (the dataset has one row per model
/// per category; `overall` is the headline Arena board).
const OVERALL_CATEGORY: &str = "overall";

/// Below this many votes, a model's Elo is treated as provisional (rendered with
/// a trailing `*`). `0` votes means "unknown", which is *not* flagged.
const PROVISIONAL_VOTE_THRESHOLD: u64 = 3000;

/// Hard cap for the fetched leaderboard body. The current feed is small; this
/// keeps a bad upstream/custom URL from turning startup into an unbounded read.
const MAX_SCORES_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCORE_ROWS: usize = 20_000;
const MAX_SCORE_FIELD_BYTES: usize = 256;

/// Bundled fallback so scores render offline / before the first successful fetch.
const BUNDLED_SNAPSHOT: &str = include_str!("scores_snapshot.json");

/// Our normalized on-disk/bundled schema. Unknown fields (e.g. `_note`) are
/// ignored by serde.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ScoresFile {
    #[serde(default)]
    pub as_of: String,
    #[serde(default)]
    pub models: Vec<ScoreRow>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ScoreRow {
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    pub elo: u32,
    #[serde(default)]
    pub votes: u64,
}

/// A resolved score ready to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelScore {
    pub elo: u32,
    pub provisional: bool,
}

/// Default on-disk cache location. The `-v2` suffix versions the source/schema
/// (parquet leaderboard); bumping it abandons caches written by an older build
/// so a format change can't be masked by a still-fresh stale cache.
pub fn default_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join("mj")
        .join("scores-v2.json")
}

// ---- loading -------------------------------------------------------------

fn cache_is_fresh(cache_path: &Path, ttl: Duration) -> bool {
    match cache_path.metadata().and_then(|m| m.modified()) {
        Ok(mtime) => SystemTime::now()
            .duration_since(mtime)
            .map(|age| age < ttl)
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn read_cache(cache_path: &Path) -> Option<ScoresFile> {
    let s = std::fs::read_to_string(cache_path).ok()?;
    parse_scores(&s).ok()
}

fn write_cache(cache_path: &Path, file: &ScoresFile) {
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(file) {
        Ok(json) => {
            let tmp = cache_path.with_file_name(format!(
                "{}.tmp",
                cache_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("scores-v2.json")
            ));
            if let Err(e) =
                std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, cache_path))
            {
                tracing::warn!("write scores cache {cache_path:?}: {e:#}");
                let _ = std::fs::remove_file(tmp);
            }
        }
        Err(e) => tracing::warn!("serialize scores cache: {e:#}"),
    }
}

/// The bundled snapshot, always parseable (it ships with the binary).
fn bundled() -> ScoresFile {
    parse_scores(BUNDLED_SNAPSHOT).expect("bundled scores snapshot must parse")
}

/// Stale cache if present and parseable, else the bundled snapshot.
fn fallback(cache_path: &Path) -> ScoresFile {
    read_cache(cache_path).unwrap_or_else(bundled)
}

/// Load the normalized scores file: fresh cache → network refresh → stale cache
/// → bundled snapshot. Never errors — there is always a usable result.
pub async fn load_scores_file(cache_path: &Path, ttl: Duration, url: &str) -> ScoresFile {
    if cache_is_fresh(cache_path, ttl)
        && let Some(file) = read_cache(cache_path)
    {
        return file;
    }

    match fetch_leaderboard(url).await {
        Ok(file) => {
            write_cache(cache_path, &file);
            file
        }
        Err(e) => {
            tracing::warn!(
                "refresh scores from {} ({e:#}); using fallback",
                redact_url(url)
            );
            fallback(cache_path)
        }
    }
}

/// Fetch and parse the leaderboard. The official source is parquet; if a config
/// override points at our own JSON schema instead, accept that too.
async fn fetch_leaderboard(url: &str) -> Result<ScoresFile> {
    let body = fetch_bytes(url).await?;
    let parquet_body = body.clone();
    match tokio::task::spawn_blocking(move || parse_parquet(parquet_body)).await {
        Ok(Ok(file)) => return Ok(file),
        Ok(Err(_)) => {}
        Err(e) => anyhow::bail!("scores parquet parser task failed: {e}"),
    }
    // Fallback: a URL serving our normalized JSON schema directly.
    let text = std::str::from_utf8(&body).context("scores body not parquet and not utf-8")?;
    parse_scores(text)
}

/// GET the raw bytes of a leaderboard file (async; reqwest is already a dep).
async fn fetch_bytes(url: &str) -> Result<bytes::Bytes> {
    let parsed =
        Url::parse(url).with_context(|| format!("parse scores URL {}", redact_url(url)))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "scores URL must use http or https"
    );
    let safe_url = redact_url(url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build http client")?;
    let resp = client
        .get(parsed)
        .send()
        .await
        .with_context(|| format!("GET {safe_url}"))?;
    let status = resp.status();
    anyhow::ensure!(status.is_success(), "GET {safe_url}: HTTP {status}");
    if let Some(len) = resp.content_length() {
        anyhow::ensure!(
            len <= MAX_SCORES_BODY_BYTES as u64,
            "scores body from {safe_url} is too large ({len} bytes)"
        );
    }

    let mut body = bytes::BytesMut::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read scores body from {safe_url}"))?;
        anyhow::ensure!(
            body.len() + chunk.len() <= MAX_SCORES_BODY_BYTES,
            "scores body from {safe_url} exceeded {MAX_SCORES_BODY_BYTES} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

fn redact_url(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return "<invalid url>".to_string();
    };
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

/// Parse the official LMArena parquet, keeping only the `overall` category and
/// mapping `model_name` / `organization` / `rating` / `vote_count`.
fn parse_parquet(body: bytes::Bytes) -> Result<ScoresFile> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let reader = ParquetRecordBatchReaderBuilder::try_new(body)
        .context("open scores parquet")?
        .build()
        .context("build parquet reader")?;

    let mut models = Vec::new();
    for batch in reader {
        let batch = batch.context("read parquet batch")?;
        let names = col_str(&batch, "model_name")?;
        let orgs = col_str(&batch, "organization")?;
        let cats = col_str(&batch, "category")?;
        let ratings = col_f64(&batch, "rating")?;
        let votes = col_f64(&batch, "vote_count")?;
        for i in 0..batch.num_rows() {
            if cats.is_null(i) || cats.value(i) != OVERALL_CATEGORY {
                continue;
            }
            if names.is_null(i) || ratings.is_null(i) {
                continue;
            }
            let name = names.value(i).to_string();
            if name.is_empty() {
                continue;
            }
            anyhow::ensure!(
                name.len() <= MAX_SCORE_FIELD_BYTES,
                "scores parquet model name too large"
            );
            let vendor = (!orgs.is_null(i)).then(|| orgs.value(i).to_string());
            if let Some(vendor) = &vendor {
                anyhow::ensure!(
                    vendor.len() <= MAX_SCORE_FIELD_BYTES,
                    "scores parquet vendor too large"
                );
            }
            let vote_count = if votes.is_null(i) {
                0.0
            } else {
                votes.value(i)
            };
            anyhow::ensure!(
                models.len() < MAX_SCORE_ROWS,
                "scores parquet has more than {MAX_SCORE_ROWS} rows"
            );
            models.push(ScoreRow {
                name,
                vendor: vendor.unwrap_or_default(),
                elo: ratings.value(i).round().max(0.0) as u32,
                votes: vote_count.max(0.0) as u64,
            });
        }
    }
    anyhow::ensure!(
        !models.is_empty(),
        "no `{OVERALL_CATEGORY}` rows in parquet"
    );
    Ok(ScoresFile {
        as_of: String::new(),
        models,
    })
}

fn col_str<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    let col = batch
        .column_by_name(name)
        .with_context(|| format!("scores parquet missing column `{name}`"))?;
    (**col)
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("scores parquet column `{name}` is not Utf8"))
}

fn col_f64<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Float64Array> {
    let col = batch
        .column_by_name(name)
        .with_context(|| format!("scores parquet missing column `{name}`"))?;
    (**col)
        .as_any()
        .downcast_ref::<Float64Array>()
        .with_context(|| format!("scores parquet column `{name}` is not Float64"))
}

/// Parse our normalized JSON schema (used for the on-disk cache, the bundled
/// snapshot, and JSON config-override URLs). Errors when it has no models.
fn parse_scores(body: &str) -> Result<ScoresFile> {
    let file: ScoresFile = serde_json::from_str(body).context("parse scores json")?;
    anyhow::ensure!(!file.models.is_empty(), "scores json has no models");
    anyhow::ensure!(
        file.models.len() <= MAX_SCORE_ROWS,
        "scores json has more than {MAX_SCORE_ROWS} models"
    );
    for row in &file.models {
        anyhow::ensure!(
            row.name.len() <= MAX_SCORE_FIELD_BYTES,
            "scores json model name too large"
        );
        anyhow::ensure!(
            row.vendor.len() <= MAX_SCORE_FIELD_BYTES,
            "scores json vendor too large"
        );
    }
    Ok(file)
}

// ---- catalog + process-global store -------------------------------------

/// Indexed scores plus the resolver config, installed once at startup and read
/// by the picker on every frame.
#[derive(Debug)]
pub struct ScoreCatalog {
    by_key: HashMap<MatchKey, ModelScore>,
    overrides: HashMap<String, String>,
    enabled: bool,
}

impl ScoreCatalog {
    /// Index a scores file by normalized match key. On key collision the
    /// higher-voted (more reliable) row wins.
    pub fn build(file: &ScoresFile, overrides: HashMap<String, String>, enabled: bool) -> Self {
        let mut indexed: HashMap<MatchKey, (ModelScore, MatchSpecificity, u64)> = HashMap::new();
        for row in &file.models {
            let score = ModelScore {
                elo: row.elo,
                provisional: row.votes != 0 && row.votes < PROVISIONAL_VOTE_THRESHOLD,
            };
            for (key, specificity) in model_resolve::lmarena_keys_ranked(&row.name, &row.vendor) {
                let replace = indexed
                    .get(&key)
                    .is_none_or(|&(_, old_specificity, old_votes)| {
                        specificity > old_specificity
                            || (specificity == old_specificity && row.votes >= old_votes)
                    });
                if replace {
                    indexed.insert(key, (score, specificity, row.votes));
                }
            }
        }
        let by_key = indexed
            .into_iter()
            .map(|(key, (score, _, _))| (key, score))
            .collect();
        Self {
            by_key,
            overrides,
            enabled,
        }
    }

    /// Exact-match lookup for one agent model option (first candidate key that
    /// hits wins). `None` when unresolved — the caller renders `—`. `name` and
    /// `description` carry the version some agents keep out of the id.
    fn lookup(
        &self,
        agent_id: &str,
        value: &str,
        name: &str,
        description: &str,
    ) -> Option<ModelScore> {
        model_resolve::agent_keys(agent_id, value, name, description, &self.overrides)
            .into_iter()
            .find_map(|key| self.by_key.get(&key).copied())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScoreStore {
    catalog: Arc<RwLock<Option<ScoreCatalog>>>,
}

impl ScoreStore {
    /// Install or replace the catalog for this UI run.
    pub fn install(&self, catalog: ScoreCatalog) {
        if let Ok(mut guard) = self.catalog.write() {
            *guard = Some(catalog);
        }
    }

    /// True when a catalog is installed and scoring is enabled — i.e. a score
    /// column is actually being rendered, so the picker should show the legend.
    pub fn is_active(&self) -> bool {
        self.catalog
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|c| c.enabled))
            .unwrap_or(false)
    }

    /// Render suffix for a model row in the picker: the Elo, or a provisional
    /// `*` variant. `None` (append nothing) when scoring is disabled / not
    /// installed, or when the model isn't on the leaderboard — an unmatched row
    /// stays bare rather than showing a placeholder dash.
    pub fn score_suffix(
        &self,
        agent_id: &str,
        value: &str,
        name: &str,
        description: &str,
    ) -> Option<String> {
        let guard = self.catalog.read().ok()?;
        let catalog = guard.as_ref()?;
        if !catalog.enabled {
            return None;
        }
        let score = catalog.lookup(agent_id, value, name, description)?;
        // The trailing `*` (on provisional ratings) stays attached to the number.
        Some(if score.provisional {
            format!("{}* elo", score.elo)
        } else {
            format!("{} elo", score.elo)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_parses_and_has_models() {
        let file = bundled();
        assert!(file.models.len() >= 10, "snapshot should seed many models");
        assert!(file.models.iter().any(|m| m.name == "claude-opus-4-8"));
    }

    fn catalog_from_snapshot(enabled: bool) -> ScoreCatalog {
        ScoreCatalog::build(&bundled(), HashMap::new(), enabled)
    }

    #[test]
    fn anvil_bedrock_id_resolves_to_real_score() {
        // The exact failing case from the screenshots: Anvil's bedrock id should
        // now resolve to the real LMArena Elo instead of `—`.
        let cat = catalog_from_snapshot(true);
        let id = "bedrock::us.anthropic.claude-opus-4-8";
        let score = cat.lookup("anvil", id, id, "");
        assert_eq!(score.map(|s| s.elo), Some(1456));
    }

    #[test]
    fn anvil_dated_bedrock_id_resolves() {
        let cat = catalog_from_snapshot(true);
        let id = "bedrock::us.anthropic.claude-opus-4-5-20251101-v1:0";
        assert_eq!(cat.lookup("anvil", id, id, "").map(|s| s.elo), Some(1450));
    }

    #[test]
    fn anvil_codex_and_zai_ids_resolve() {
        let cat = catalog_from_snapshot(true);
        assert_eq!(
            cat.lookup("anvil", "codex::gpt-5.5", "codex::gpt-5.5", "")
                .map(|s| s.elo),
            Some(1463)
        );
        assert_eq!(
            cat.lookup("anvil", "bedrock::zai.glm-5", "bedrock::zai.glm-5", "")
                .map(|s| s.elo),
            Some(1446)
        );
    }

    #[test]
    fn claude_acp_alias_resolves_via_description() {
        // The exact screenshot case: value `opus`, name `Opus`, version only in
        // the description.
        let cat = catalog_from_snapshot(true);
        let score = cat.lookup(
            "claude-acp",
            "opus",
            "Opus",
            "Opus 4.8 with 1M context · Best for everyday, complex tasks",
        );
        assert_eq!(
            score,
            Some(ModelScore {
                elo: 1456,
                provisional: false
            })
        );
        // Sonnet and Haiku from the same picker.
        assert_eq!(
            cat.lookup(
                "claude-acp",
                "sonnet",
                "Sonnet",
                "Sonnet 4.6 · Efficient for routine tasks"
            )
            .map(|s| s.elo),
            Some(1457)
        );
    }

    #[test]
    fn catalog_joins_aggregator_provider_model_id() {
        let cat = catalog_from_snapshot(true);
        let score = cat.lookup(
            "opencode",
            "anthropic/claude-opus-4-8",
            "Claude Opus 4.8",
            "",
        );
        assert_eq!(score.map(|s| s.elo), Some(1456));
    }

    #[test]
    fn provisional_flag_set_from_low_vote_count() {
        // Build a catalog with one low-vote row; it must be flagged provisional.
        let file = ScoresFile {
            as_of: String::new(),
            models: vec![ScoreRow {
                name: "fresh-model-x".to_string(),
                vendor: "anthropic".to_string(),
                elo: 1400,
                votes: 800, // < PROVISIONAL_VOTE_THRESHOLD
            }],
        };
        let cat = ScoreCatalog::build(&file, HashMap::new(), true);
        let score = cat
            .lookup("claude-acp", "fresh-model-x", "fresh-model-x", "")
            .unwrap();
        assert!(score.provisional);
        // The bundled glm-4.6v (2806 votes) is also provisional.
        let snap = catalog_from_snapshot(true);
        assert!(
            snap.lookup(
                "anvil",
                "bedrock::zai.glm-4.6v",
                "bedrock::zai.glm-4.6v",
                ""
            )
            .unwrap()
            .provisional
        );
    }

    #[test]
    fn unmatched_model_has_no_score() {
        let cat = catalog_from_snapshot(true);
        assert!(cat.lookup("devin", "devin-1", "Devin 1", "").is_none());
    }

    #[test]
    fn disabled_catalog_yields_no_suffix() {
        // A disabled catalog never decorates rows.
        let cat = catalog_from_snapshot(false);
        assert!(!cat.enabled);
    }

    #[test]
    fn override_redirects_to_base_model() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "codex-acp/gpt-5-codex".to_string(),
            "openai/gpt-5.5".to_string(),
        );
        let cat = ScoreCatalog::build(&bundled(), overrides, true);
        let score = cat.lookup("codex-acp", "gpt-5-codex", "GPT-5 Codex", "");
        assert_eq!(score.map(|s| s.elo), Some(1463));
    }

    #[test]
    fn exact_base_row_beats_higher_vote_variant_alias() {
        let file = ScoresFile {
            as_of: String::new(),
            models: vec![
                ScoreRow {
                    name: "gpt-5.5".to_string(),
                    vendor: "openai".to_string(),
                    elo: 1463,
                    votes: 100,
                },
                ScoreRow {
                    name: "gpt-5.5-high".to_string(),
                    vendor: "openai".to_string(),
                    elo: 1468,
                    votes: 10_000,
                },
            ],
        };
        let cat = ScoreCatalog::build(&file, HashMap::new(), true);
        assert_eq!(
            cat.lookup("codex-acp", "gpt-5.5", "GPT-5.5", "")
                .map(|s| s.elo),
            Some(1463)
        );
        assert_eq!(
            cat.lookup("codex-acp", "gpt-5.5-high", "GPT-5.5 High", "")
                .map(|s| s.elo),
            Some(1468)
        );
    }

    #[test]
    fn parse_accepts_our_schema() {
        let body = r#"{"as_of":"2026-01-01","models":[{"name":"gpt-5.5","vendor":"openai","elo":1463,"votes":34794}]}"#;
        let file = parse_scores(body).expect("our schema");
        assert_eq!(file.models.len(), 1);
        assert_eq!(file.models[0].elo, 1463);
    }

    #[test]
    fn parse_rejects_unrelated_json() {
        assert!(parse_scores(r#"{"hello":"world"}"#).is_err());
    }

    #[test]
    fn redacted_url_removes_credentials_query_and_fragment() {
        assert_eq!(
            redact_url("https://user:secret@example.com/private/feed.parquet?token=abc#frag"),
            "https://example.com/private/feed.parquet"
        );
    }

    #[test]
    fn parse_parquet_keeps_overall_and_maps_columns() {
        use arrow_schema::{DataType, Field, Schema};
        use parquet::arrow::ArrowWriter;
        use std::sync::Arc;

        let schema = Arc::new(Schema::new(vec![
            Field::new("model_name", DataType::Utf8, false),
            Field::new("organization", DataType::Utf8, true),
            Field::new("rating", DataType::Float64, false),
            Field::new("vote_count", DataType::Float64, false),
            Field::new("category", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec![
                    "claude-opus-4-8",
                    "claude-opus-4-8",
                    "glm-5",
                ])),
                Arc::new(StringArray::from(vec![
                    Some("anthropic"),
                    Some("anthropic"),
                    Some("zai"),
                ])),
                Arc::new(Float64Array::from(vec![1456.4, 999.0, 1446.0])),
                Arc::new(Float64Array::from(vec![19038.0, 5.0, 26794.0])),
                Arc::new(StringArray::from(vec!["overall", "chinese", "overall"])),
            ],
        )
        .unwrap();

        let mut buf = Vec::new();
        {
            let mut w = ArrowWriter::try_new(&mut buf, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }

        let file = parse_parquet(bytes::Bytes::from(buf)).expect("parse parquet");
        // Only the two `overall` rows survive; the `chinese` row is dropped.
        assert_eq!(file.models.len(), 2);
        let opus = file
            .models
            .iter()
            .find(|m| m.name == "claude-opus-4-8")
            .unwrap();
        assert_eq!(opus.elo, 1456); // 1456.4 rounded
        assert_eq!(opus.vendor, "anthropic");
        assert_eq!(opus.votes, 19038);
        // And the parsed parquet resolves end-to-end for an Anvil id.
        let cat = ScoreCatalog::build(&file, HashMap::new(), true);
        let id = "bedrock::us.anthropic.claude-opus-4-8";
        assert_eq!(cat.lookup("anvil", id, id, "").map(|s| s.elo), Some(1456));
    }
}
