//! Model strength scores (LMArena Elo) — fetch + cache + process-global store.
//!
//! Mirrors [`crate::registry`]: a `load_*` function refreshes the leaderboard
//! from a URL, caches a normalized copy under `~/.cache/mj/scores.json`, and
//! refreshes only when the cached copy is older than [`CACHE_TTL`].
//! Offline-friendly with a belt-and-braces fallback chain: fresh cache → network
//! → stale cache → **bundled snapshot** compiled into the binary, so the picker
//! always has *something* to show.
//!
//! The runtime source is the **official LMArena leaderboard dataset** through
//! Hugging Face's dataset viewer JSON API ([`DEFAULT_SCORES_URL`]). The API
//! filters the upstream dataset to the overall board and serves bounded pages;
//! mj normalizes those rows to the small JSON schema used by the cache.
//!
//! Scores are joined onto an agent's selectable models via [`crate::model_resolve`]:
//! each leaderboard row and each model option are reduced to the same normalized
//! match key. No key match → no score (the picker renders `—`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::model_resolve::{self, MatchKey, MatchSpecificity};

/// How long a cached scores copy is considered fresh. New frontier models reach
/// LMArena within ~24–48h; weekly polling is ample for "new models show up".
pub const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Official LMArena leaderboard dataset, `text` subset / `latest` split,
/// filtered to the overall Chatbot Arena board by Hugging Face's dataset
/// viewer. JSON over HTTPS, no auth, redistributable. Overridable with a URL
/// serving mj's normalized [`ScoresFile`] schema (`[scores] url = …`).
pub const DEFAULT_SCORES_URL: &str = "https://datasets-server.huggingface.co/filter?dataset=lmarena-ai%2Fleaderboard-dataset&config=text&split=latest&where=%22category%22%3D%27overall%27";

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
const DATASET_PAGE_ROWS: usize = 100;

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

/// Default on-disk cache location. The `-v2` suffix versions the normalized
/// JSON schema; the live transport can change without invalidating compatible
/// cached score rows.
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

/// Fetch and parse the leaderboard. The official URL uses the paginated
/// Hugging Face dataset API; configured overrides serve mj's normalized JSON.
async fn fetch_leaderboard(url: &str) -> Result<ScoresFile> {
    let client = scores_http_client()?;
    if url == DEFAULT_SCORES_URL {
        return fetch_dataset_leaderboard(&client, url).await;
    }

    let body = fetch_bytes(&client, Url::parse(url).context("parse scores URL")?).await?;
    let text = std::str::from_utf8(&body).context("scores JSON body is not utf-8")?;
    parse_scores(text)
}

fn scores_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(concat!("mj/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build scores http client")
}

/// GET one bounded JSON response (async; reqwest is already a dependency).
async fn fetch_bytes(client: &reqwest::Client, parsed: Url) -> Result<Vec<u8>> {
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "scores URL must use http or https"
    );
    let safe_url = redact_url(parsed.as_str());
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

    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("read scores body from {safe_url}"))?;
        anyhow::ensure!(
            body.len() + chunk.len() <= MAX_SCORES_BODY_BYTES,
            "scores body from {safe_url} exceeded {MAX_SCORES_BODY_BYTES} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Debug, Deserialize)]
struct DatasetRowsPage {
    rows: Vec<DatasetRowEnvelope>,
    num_rows_total: usize,
    #[serde(default)]
    partial: bool,
}

#[derive(Debug, Deserialize)]
struct DatasetRowEnvelope {
    row: DatasetScoreRow,
}

#[derive(Debug, Deserialize)]
struct DatasetScoreRow {
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    organization: Option<String>,
    rating: Option<f64>,
    vote_count: Option<f64>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    leaderboard_publish_date: Option<String>,
}

async fn fetch_dataset_leaderboard(client: &reqwest::Client, base_url: &str) -> Result<ScoresFile> {
    let mut offset = 0;
    let mut total = None;
    let mut models = Vec::new();
    let mut as_of = String::new();

    loop {
        let page_url = dataset_page_url(base_url, offset)?;
        let body = fetch_bytes(client, page_url).await?;
        let page = parse_dataset_page(&body, offset)?;
        anyhow::ensure!(
            page.num_rows_total <= MAX_SCORE_ROWS,
            "scores dataset has more than {MAX_SCORE_ROWS} rows"
        );
        if let Some(expected) = total {
            anyhow::ensure!(
                page.num_rows_total == expected,
                "scores dataset size changed during pagination"
            );
        } else {
            total = Some(page.num_rows_total);
        }

        let page_len = page.rows.len();
        append_dataset_rows(page.rows, &mut models, &mut as_of)?;
        offset += page_len;
        if offset >= page.num_rows_total {
            break;
        }
        anyhow::ensure!(page_len != 0, "scores dataset pagination made no progress");
    }

    anyhow::ensure!(!models.is_empty(), "scores dataset has no overall models");
    Ok(ScoresFile { as_of, models })
}

fn parse_dataset_page(body: &[u8], offset: usize) -> Result<DatasetRowsPage> {
    let page: DatasetRowsPage = serde_json::from_slice(body)
        .with_context(|| format!("parse scores dataset page at offset {offset}"))?;
    anyhow::ensure!(
        !page.partial,
        "scores dataset API returned a partial result"
    );
    Ok(page)
}

fn dataset_page_url(base_url: &str, offset: usize) -> Result<Url> {
    let mut url = Url::parse(base_url)
        .with_context(|| format!("parse scores URL {}", redact_url(base_url)))?;
    let retained = url
        .query_pairs()
        .filter(|(key, _)| key != "offset" && key != "length")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(retained)
        .append_pair("offset", &offset.to_string())
        .append_pair("length", &DATASET_PAGE_ROWS.to_string());
    Ok(url)
}

fn append_dataset_rows(
    rows: Vec<DatasetRowEnvelope>,
    models: &mut Vec<ScoreRow>,
    as_of: &mut String,
) -> Result<()> {
    for envelope in rows {
        let row = envelope.row;
        if row.category.as_deref() != Some(OVERALL_CATEGORY) {
            continue;
        }
        let Some(name) = row.model_name.filter(|name| !name.is_empty()) else {
            continue;
        };
        let vendor = row.organization.unwrap_or_default();
        let Some(rating) = row.rating.filter(|value| value.is_finite()) else {
            continue;
        };
        anyhow::ensure!(
            name.len() <= MAX_SCORE_FIELD_BYTES,
            "scores dataset model name too large"
        );
        anyhow::ensure!(
            vendor.len() <= MAX_SCORE_FIELD_BYTES,
            "scores dataset vendor too large"
        );
        anyhow::ensure!(
            models.len() < MAX_SCORE_ROWS,
            "scores dataset has more than {MAX_SCORE_ROWS} rows"
        );
        if let Some(publish_date) = row.leaderboard_publish_date
            && publish_date > *as_of
        {
            *as_of = publish_date;
        }
        models.push(ScoreRow {
            name,
            vendor,
            elo: rating.round().max(0.0) as u32,
            votes: row
                .vote_count
                .filter(|value| value.is_finite())
                .unwrap_or(0.0)
                .max(0.0) as u64,
        });
    }
    Ok(())
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
        self.lookup_with_key(agent_id, value, name, description)
            .map(|(_, score)| score)
    }

    /// Like [`Self::lookup`], but also returns the normalized match key that
    /// resolved. Two options resolving to the same key are the same underlying
    /// model, regardless of which agent exposes them — Ragnarok uses this to
    /// keep its roster of champions genuinely distinct.
    fn lookup_with_key(
        &self,
        agent_id: &str,
        value: &str,
        name: &str,
        description: &str,
    ) -> Option<(MatchKey, ModelScore)> {
        model_resolve::agent_keys(agent_id, value, name, description, &self.overrides)
            .into_iter()
            .find_map(|key| self.by_key.get(&key).copied().map(|score| (key, score)))
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

    /// True when any catalog has been installed, regardless of the picker
    /// display toggle. Ragnarok loads its own catalog when this is false.
    pub fn has_catalog(&self) -> bool {
        self.catalog
            .read()
            .ok()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Numeric Elo lookup for one agent model option, plus the normalized
    /// match key it resolved through. Unlike [`Self::score_suffix`] this
    /// ignores the display toggle: an installed catalog always answers.
    /// `None` means the model is not on the leaderboard — for Ragnarok that
    /// makes it ineligible.
    pub fn model_score_with_key(
        &self,
        agent_id: &str,
        value: &str,
        name: &str,
        description: &str,
    ) -> Option<(MatchKey, ModelScore)> {
        let guard = self.catalog.read().ok()?;
        guard
            .as_ref()?
            .lookup_with_key(agent_id, value, name, description)
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
            redact_url("https://user:secret@example.com/private/scores.json?token=abc#frag"),
            "https://example.com/private/scores.json"
        );
    }

    #[test]
    fn dataset_rows_keep_overall_models_and_map_fields() {
        let body = r#"{
            "rows": [
                {"row":{"model_name":"claude-opus-4-8","organization":"anthropic","rating":1456.4,"vote_count":19038.0,"category":"overall","leaderboard_publish_date":"2026-07-02"}},
                {"row":{"model_name":"claude-opus-4-8","organization":"anthropic","rating":999.0,"vote_count":5.0,"category":"chinese","leaderboard_publish_date":"2026-07-02"}},
                {"row":{"model_name":"glm-5","organization":"zai","rating":1446.0,"vote_count":26794.0,"category":"overall","leaderboard_publish_date":"2026-06-25"}}
            ],
            "num_rows_total": 3,
            "partial": false
        }"#;
        let page: DatasetRowsPage = serde_json::from_str(body).expect("dataset page");
        let mut models = Vec::new();
        let mut as_of = String::new();
        append_dataset_rows(page.rows, &mut models, &mut as_of).expect("map rows");

        assert_eq!(models.len(), 2);
        assert_eq!(as_of, "2026-07-02");
        let opus = models
            .iter()
            .find(|model| model.name == "claude-opus-4-8")
            .unwrap();
        assert_eq!(opus.elo, 1456);
        assert_eq!(opus.vendor, "anthropic");
        assert_eq!(opus.votes, 19038);

        let file = ScoresFile { as_of, models };
        let cat = ScoreCatalog::build(&file, HashMap::new(), true);
        let id = "bedrock::us.anthropic.claude-opus-4-8";
        assert_eq!(cat.lookup("anvil", id, id, "").map(|s| s.elo), Some(1456));
    }

    #[test]
    fn dataset_page_url_replaces_pagination_parameters() {
        let url = dataset_page_url(
            "https://datasets-server.huggingface.co/filter?dataset=arena&offset=9&length=1",
            200,
        )
        .expect("page url");
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            pairs.get("dataset").map(|value| value.as_ref()),
            Some("arena")
        );
        assert_eq!(pairs.get("offset").map(|value| value.as_ref()), Some("200"));
        assert_eq!(pairs.get("length").map(|value| value.as_ref()), Some("100"));
    }

    #[test]
    fn dataset_page_rejects_partial_results() {
        assert!(
            parse_dataset_page(
                r#"{"rows":[],"num_rows_total":1,"partial":true}"#.as_bytes(),
                0
            )
            .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires the live Hugging Face dataset API"]
    async fn default_dataset_api_loads_current_overall_scores() {
        let client = scores_http_client().expect("http client");
        let file = fetch_dataset_leaderboard(&client, DEFAULT_SCORES_URL)
            .await
            .expect("live leaderboard");
        assert!(file.models.len() > DATASET_PAGE_ROWS);
        assert!(!file.as_of.is_empty());
        assert!(file.models.iter().any(|model| model.vendor == "anthropic"));
    }
}
