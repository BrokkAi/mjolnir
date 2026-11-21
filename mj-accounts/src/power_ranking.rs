//! Compiled-in power ranking, plus a 24-hour disk cache of the published table.
//!
//! Selection never waits on the network. The daemon loads this store at startup
//! and refreshes it on a background task.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const DEFAULT_URL: &str = "https://slopcop.com/power-ranking.json";
const FRESH_FOR_SECONDS: u64 = 86_400;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// One ranked `(model, effort, harness)` row from the published table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PowerRankingEntry {
    pub config: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub tasks_per_200_sub: Option<f64>,
    #[serde(default)]
    pub tasks_per_200_api: Option<f64>,
    #[serde(default)]
    pub weekly_budget_usd: Option<f64>,
    #[serde(default)]
    pub plan_usd_per_month: Option<f64>,
    #[serde(default)]
    pub plan_assumed: bool,
    pub basis: String,
    #[serde(default)]
    pub minutes: Option<f64>,
}

/// The published table. `schema_version` 1 is the only shape this build reads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PowerRankingSnapshot {
    pub schema_version: u32,
    /// Publication date. `null` or a missing field is absent, not an error.
    #[serde(default)]
    pub as_of: Option<String>,
    /// Commit that produced the table. `null` or a missing field is absent.
    #[serde(default)]
    pub source_commit: Option<String>,
    pub entries: Vec<PowerRankingEntry>,
}

/// A row chosen for a model, or the geometric mean of its ranked efforts.
#[derive(Debug, Clone, PartialEq)]
pub struct PowerRankingLookup {
    pub model: String,
    pub effort: Option<String>,
    pub harness: Option<String>,
    pub tasks_per_200_sub: Option<f64>,
    pub tasks_per_200_api: Option<f64>,
    pub weekly_budget_usd: Option<f64>,
    pub plan_usd_per_month: Option<f64>,
    pub plan_assumed: bool,
    pub basis: String,
    pub minutes: Option<f64>,
    /// Set when no ranked effort matched and the numbers are a geometric mean.
    pub geometric_mean: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PowerRankingCacheFile {
    mj_version: String,
    fetched_at_epoch_seconds: u64,
    snapshot: PowerRankingSnapshot,
}

struct Ranked {
    snapshot: PowerRankingSnapshot,
    by_model: BTreeMap<String, Vec<PowerRankingEntry>>,
}

struct State {
    ranked: Ranked,
    /// `None` when the in-memory table is the compiled baseline, not a cache.
    fetched_at_epoch_seconds: Option<u64>,
    mj_version: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    cache_path: PathBuf,
    url: String,
}

/// The ranking table selection reads. Clone is cheap; the table sits behind one lock.
#[derive(Clone)]
pub struct PowerRankingStore {
    shared: Arc<Shared>,
}

impl PowerRankingStore {
    /// Load a fresh same-version disk cache, or the compiled-in baseline.
    /// Does not use the network. `cache_dir` is the directory that holds
    /// `power-ranking-cache.json` (the daemon passes [`mj_core::config::data_dir`]).
    pub fn open(cache_dir: &Path, now_seconds: u64) -> Self {
        let url = std::env::var("MJ_POWER_RANKING_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
        Self::with_url(cache_dir, now_seconds, url)
    }

    /// [`open`] with an explicit fetch URL, for tests that must not read the process environment.
    pub fn with_url(cache_dir: &Path, now_seconds: u64, url: String) -> Self {
        let cache_path = cache_dir.join("power-ranking-cache.json");
        let state = load_state(&cache_path, now_seconds);
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                cache_path,
                url,
            }),
        }
    }

    pub fn snapshot(&self) -> PowerRankingSnapshot {
        self.lock().ranked.snapshot.clone()
    }

    /// Match `model_identity` and `effort` for `harness`.
    ///
    /// A harness-tagged row wins over an untagged row for the same model. An
    /// untagged row is not replaced by a different harness's tag. A missing or
    /// unranked effort falls back to `high`, then `medium`, then the geometric
    /// mean of the ranked efforts for that model and harness.
    pub fn lookup(
        &self,
        model_identity: &str,
        effort: Option<&str>,
        harness: mj_core::config::HarnessKind,
    ) -> Option<PowerRankingLookup> {
        let state = self.lock();
        let entries = model_aliases(model_identity).into_iter().find_map(|alias| {
            state.ranked.by_model.get(&alias)
        })?;
        let matched = entries_for_harness(entries, harness);
        if matched.is_empty() {
            return None;
        }
        pick_effort(&matched, effort.map(str::to_ascii_lowercase))
    }

    /// Fetch when the disk cache is missing, older than 24 hours, or from another `mj` version.
    /// A failed fetch keeps the current snapshot.
    pub async fn refresh(&self, now_seconds: u64) -> Result<()> {
        if self.lock().is_fresh(now_seconds) {
            return Ok(());
        }
        let url = self.shared.url.clone();
        let fetched = match fetch_snapshot(&url).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "power-ranking refresh failed; keeping the current snapshot"
                );
                return Err(error);
            }
        };
        let cache = PowerRankingCacheFile {
            mj_version: env!("CARGO_PKG_VERSION").to_owned(),
            fetched_at_epoch_seconds: now_seconds,
            snapshot: fetched,
        };
        let write_result = write_cache(&self.shared.cache_path, &cache);
        let mut state = self.lock();
        state.fetched_at_epoch_seconds = Some(now_seconds);
        state.mj_version = Some(cache.mj_version);
        state.ranked = Ranked::new(cache.snapshot);
        drop(state);
        if let Err(error) = write_result {
            tracing::warn!(
                %error,
                path = %self.shared.cache_path.display(),
                "power-ranking cache write failed; updated in-memory snapshot"
            );
            return Err(error);
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl State {
    fn is_fresh(&self, now_seconds: u64) -> bool {
        self.mj_version.as_deref() == Some(env!("CARGO_PKG_VERSION"))
            && self
                .fetched_at_epoch_seconds
                .is_some_and(|fetched| now_seconds.saturating_sub(fetched) < FRESH_FOR_SECONDS)
    }
}

impl Ranked {
    fn new(snapshot: PowerRankingSnapshot) -> Self {
        let mut by_model: BTreeMap<String, Vec<PowerRankingEntry>> = BTreeMap::new();
        for entry in &snapshot.entries {
            by_model
                .entry(entry.model.clone())
                .or_default()
                .push(entry.clone());
        }
        // `opus/xhigh` is published as model `claude-opus-5`. Index that
        // shorter config spelling too, unless it already names another model.
        for entry in &snapshot.entries {
            let Some(stem) = entry.config.split('/').next() else {
                continue;
            };
            if stem.is_empty() || stem == entry.model {
                continue;
            }
            let bucket = by_model.entry(stem.to_owned()).or_default();
            if bucket.iter().any(|existing| existing.model != entry.model) {
                continue;
            }
            bucket.push(entry.clone());
        }
        Self { snapshot, by_model }
    }
}

fn baseline() -> &'static PowerRankingSnapshot {
    static BASELINE: OnceLock<PowerRankingSnapshot> = OnceLock::new();
    BASELINE.get_or_init(|| {
        match serde_json::from_str(include_str!("../assets/power-ranking-baseline.json")) {
            Ok(snapshot) => snapshot,
            Err(error) => panic!("compiled power-ranking baseline is invalid: {error}"),
        }
    })
}

fn load_state(cache_path: &Path, now_seconds: u64) -> State {
    if let Some(cache) = read_cache(cache_path)
        && cache.mj_version == env!("CARGO_PKG_VERSION")
    {
        // A stale same-version cache stays in memory so a failed refresh
        // keeps the last good table. A timestamp in the future is clamped
        // so a skewed clock cannot keep the cache fresh forever.
        return State {
            ranked: Ranked::new(cache.snapshot),
            fetched_at_epoch_seconds: Some(cache.fetched_at_epoch_seconds.min(now_seconds)),
            mj_version: Some(cache.mj_version),
        };
    }
    State {
        ranked: Ranked::new(baseline().clone()),
        fetched_at_epoch_seconds: None,
        mj_version: None,
    }
}

fn read_cache(path: &Path) -> Option<PowerRankingCacheFile> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(cache) => Some(cache),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "ignoring unreadable power-ranking cache");
            None
        }
    }
}

fn write_cache(path: &Path, cache: &PowerRankingCacheFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_vec(cache).context("encode power-ranking cache")?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, body)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

async fn fetch_snapshot(url: &str) -> Result<PowerRankingSnapshot> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(format!("mj/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build the power-ranking client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetch {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("power-ranking fetch returned {status}");
    }
    let snapshot: PowerRankingSnapshot = response
        .json()
        .await
        .context("decode power-ranking JSON")?;
    if snapshot.schema_version != 1 {
        bail!(
            "power-ranking schema_version {} is not supported",
            snapshot.schema_version
        );
    }
    Ok(snapshot)
}

fn entries_for_harness(
    entries: &[PowerRankingEntry],
    harness: mj_core::config::HarnessKind,
) -> Vec<PowerRankingEntry> {
    let tag = harness.id();
    let tagged: Vec<_> = entries
        .iter()
        .filter(|entry| entry.harness.as_deref() == Some(tag))
        .cloned()
        .collect();
    if !tagged.is_empty() {
        return tagged;
    }
    entries
        .iter()
        .filter(|entry| entry.harness.is_none())
        .cloned()
        .collect()
}

fn pick_effort(entries: &[PowerRankingEntry], effort: Option<String>) -> Option<PowerRankingLookup> {
    if let Some(effort) = effort.as_deref()
        && let Some(found) = entries.iter().find(|entry| entry.effort.as_deref() == Some(effort))
    {
        return Some(lookup_from(found, false));
    }
    for fallback in ["high", "medium"] {
        if let Some(found) = entries
            .iter()
            .find(|entry| entry.effort.as_deref() == Some(fallback))
        {
            return Some(lookup_from(found, false));
        }
    }
    geometric_mean(entries)
}

fn lookup_from(entry: &PowerRankingEntry, geometric_mean: bool) -> PowerRankingLookup {
    PowerRankingLookup {
        model: entry.model.clone(),
        effort: entry.effort.clone(),
        harness: entry.harness.clone(),
        tasks_per_200_sub: entry.tasks_per_200_sub,
        tasks_per_200_api: entry.tasks_per_200_api,
        weekly_budget_usd: entry.weekly_budget_usd,
        plan_usd_per_month: entry.plan_usd_per_month,
        plan_assumed: entry.plan_assumed,
        basis: entry.basis.clone(),
        minutes: entry.minutes,
        geometric_mean,
    }
}

fn geometric_mean(entries: &[PowerRankingEntry]) -> Option<PowerRankingLookup> {
    let first = entries.first()?;
    Some(PowerRankingLookup {
        model: first.model.clone(),
        effort: None,
        harness: first.harness.clone(),
        tasks_per_200_sub: mean_of(entries.iter().filter_map(|entry| entry.tasks_per_200_sub)),
        tasks_per_200_api: mean_of(entries.iter().filter_map(|entry| entry.tasks_per_200_api)),
        weekly_budget_usd: mean_of(entries.iter().filter_map(|entry| entry.weekly_budget_usd)),
        plan_usd_per_month: mean_of(entries.iter().filter_map(|entry| entry.plan_usd_per_month)),
        plan_assumed: entries.iter().any(|entry| entry.plan_assumed),
        basis: "geometric-mean".to_owned(),
        minutes: mean_of(entries.iter().filter_map(|entry| entry.minutes)),
        geometric_mean: true,
    })
}

fn mean_of(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut count = 0usize;
    let mut log_sum = 0.0;
    for value in values {
        if value.is_finite() && value > 0.0 {
            log_sum += value.ln();
            count += 1;
        }
    }
    (count > 0).then(|| (log_sum / count as f64).exp())
}

/// Query forms that should hit one stored model id.
fn model_aliases(model: &str) -> Vec<String> {
    let normalized = strip_context_hints(&mj_core::model_catalog::normalize_model(model));
    // Kimi advertises `kimi-code/k3.1-pro`; the table stores `kimi-k3.1-pro`.
    let normalized = normalized
        .strip_prefix("kimi-code/")
        .unwrap_or(normalized.as_str())
        .to_owned();
    let stripped = normalized
        .strip_suffix("-dsh")
        .or_else(|| normalized.strip_suffix("-mj"))
        .unwrap_or(normalized.as_str())
        .to_owned();
    let mut keys = Vec::new();
    let mut push = |key: String| {
        if !key.is_empty() && !keys.iter().any(|existing| existing == &key) {
            keys.push(key);
        }
    };
    push(stripped.clone());
    if let Some(body) = claude_body(&stripped) {
        let swapped = swap_digit_separators(body);
        for form in [body.to_owned(), swapped] {
            push(format!("claude-{form}"));
            push(form);
        }
    }
    if is_kimi(&stripped) {
        let body = stripped.strip_prefix("kimi-").unwrap_or(&stripped);
        let swapped = swap_digit_separators(body);
        for form in [body.to_owned(), swapped] {
            push(format!("kimi-{form}"));
            push(form);
        }
    }
    keys
}

/// Drop context-window hints such as `[1m]` from `claude-opus-5-5[1m]`.
fn strip_context_hints(model: &str) -> String {
    let mut out = String::with_capacity(model.len());
    let mut rest = model;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        match rest[start + 1..].find(']') {
            Some(end) => rest = &rest[start + 1 + end + 1..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn claude_body(model: &str) -> Option<&str> {
    let body = model.strip_prefix("claude-").unwrap_or(model);
    let family = body.split(['-', '.']).next().unwrap_or(body);
    matches!(family, "opus" | "sonnet" | "haiku" | "fable" | "mythos").then_some(body)
}

fn is_kimi(model: &str) -> bool {
    let body = model.strip_prefix("kimi-").unwrap_or(model);
    body.starts_with('k') && body.chars().nth(1).is_some_and(|ch| ch.is_ascii_digit())
}

/// `5.5` and `5-5` name the same Claude or Kimi version. Other hyphens stay.
fn swap_digit_separators(model: &str) -> String {
    let chars: Vec<char> = model.chars().collect();
    let mut swapped = String::with_capacity(model.len());
    for (index, ch) in chars.iter().enumerate() {
        let between_digits = index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit();
        swapped.push(match (*ch, between_digits) {
            ('.', true) => '-',
            ('-', true) => '.',
            (ch, _) => ch,
        });
    }
    swapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::HarnessKind;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn close_enough(actual: Option<f64>, expected: f64) -> bool {
        actual.is_some_and(|value| (value - expected).abs() < 0.001)
    }

    #[tokio::test]
    async fn power_ranking_cache_freshness_and_canonical_lookups() {
        let now = 2_000_000_u64;
        let offline = tempfile::tempdir().unwrap();
        let baseline = PowerRankingStore::with_url(
            offline.path(),
            now,
            "http://127.0.0.1:1/power-ranking.json".into(),
        );
        let opus = baseline
            .lookup("opus-5.5", Some("xhigh"), HarnessKind::Claude)
            .expect("baseline opus");
        assert!(close_enough(opus.tasks_per_200_sub, 548.0));
        assert!(!opus.geometric_mean);
        assert_canonical_lookups(&baseline);

        let hits = Arc::new(AtomicUsize::new(0));
        let published = Arc::new(Mutex::new((200_u16, published_body(900.0))));
        let url = spawn_ranking_server(hits.clone(), published.clone());

        let fresh_dir = tempfile::tempdir().unwrap();
        write_cache_file(
            fresh_dir.path(),
            env!("CARGO_PKG_VERSION"),
            now - 60,
            sentinel_snapshot(42.0),
        );
        // `open` reads the override. A guard clears it if a later assert fails,
        // so the process-global variable does not leak into another test.
        struct ClearPowerRankingUrl;
        impl Drop for ClearPowerRankingUrl {
            fn drop(&mut self) {
                // SAFETY: this test is the only reader of the variable, and
                // the guard removes it before the test returns.
                unsafe { std::env::remove_var("MJ_POWER_RANKING_URL") };
            }
        }
        let _clear_url = ClearPowerRankingUrl;
        // SAFETY: same as the guard. `open` is what reads the override.
        unsafe { std::env::set_var("MJ_POWER_RANKING_URL", &url) };
        let fresh = PowerRankingStore::open(fresh_dir.path(), now);
        let hit = fresh
            .lookup("sentinel-model", Some("high"), HarnessKind::Codex)
            .expect("fresh cache");
        assert!(close_enough(hit.tasks_per_200_sub, 42.0));
        fresh.refresh(now).await.expect("fresh cache skips the network");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a fresh cache is not fetched");

        let stale_dir = tempfile::tempdir().unwrap();
        write_cache_file(
            stale_dir.path(),
            env!("CARGO_PKG_VERSION"),
            now - FRESH_FOR_SECONDS,
            sentinel_snapshot(7.0),
        );
        let stale = PowerRankingStore::with_url(stale_dir.path(), now, url.clone());
        assert!(close_enough(
            stale
                .lookup("sentinel-model", Some("high"), HarnessKind::Codex)
                .unwrap()
                .tasks_per_200_sub,
            7.0
        ));
        stale.refresh(now).await.expect("expired cache refreshes");
        assert!(hits.load(Ordering::SeqCst) >= 1);
        assert!(close_enough(
            stale
                .lookup("sentinel-model", Some("high"), HarnessKind::Codex)
                .unwrap()
                .tasks_per_200_sub,
            900.0
        ));

        let version_dir = tempfile::tempdir().unwrap();
        write_cache_file(
            version_dir.path(),
            "0.0.0-not-this",
            now - 10,
            sentinel_snapshot(1234.0),
        );
        let invalidated = PowerRankingStore::with_url(version_dir.path(), now, url.clone());
        assert!(
            invalidated
                .lookup("sentinel-model", Some("high"), HarnessKind::Codex)
                .is_none(),
            "a different mj version does not keep the old snapshot"
        );
        assert!(close_enough(
            invalidated
                .lookup("opus-5.5", Some("xhigh"), HarnessKind::Claude)
                .unwrap()
                .tasks_per_200_sub,
            548.0
        ));
        let before = hits.load(Ordering::SeqCst);
        invalidated.refresh(now).await.expect("version mismatch refreshes");
        assert!(hits.load(Ordering::SeqCst) > before);
        assert!(close_enough(
            invalidated
                .lookup("sentinel-model", Some("high"), HarnessKind::Codex)
                .unwrap()
                .tasks_per_200_sub,
            900.0
        ));

        let retained_dir = tempfile::tempdir().unwrap();
        write_cache_file(
            retained_dir.path(),
            env!("CARGO_PKG_VERSION"),
            now - FRESH_FOR_SECONDS - 5,
            sentinel_snapshot(55.0),
        );
        *published.lock().unwrap() = (500, "nope".into());
        let retained = PowerRankingStore::with_url(retained_dir.path(), now, url.clone());
        let error = retained.refresh(now).await.expect_err("HTTP 500");
        assert!(error.to_string().contains("500"), "{error}");
        assert!(close_enough(
            retained
                .lookup("sentinel-model", Some("high"), HarnessKind::Codex)
                .unwrap()
                .tasks_per_200_sub,
            55.0
        ));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let refused = listener.local_addr().unwrap();
        drop(listener);
        let unreachable = PowerRankingStore::with_url(
            retained_dir.path(),
            now,
            format!("http://{refused}/power-ranking.json"),
        );
        assert!(unreachable.refresh(now).await.is_err());
        assert!(close_enough(
            unreachable
                .lookup("sentinel-model", Some("high"), HarnessKind::Codex)
                .unwrap()
                .tasks_per_200_sub,
            55.0
        ));
    }

    fn assert_canonical_lookups(store: &PowerRankingStore) {
        let opus = store
            .lookup("claude-opus-5-5", Some("xhigh"), HarnessKind::Claude)
            .unwrap();
        assert_eq!(opus.model, "claude-opus-5-5");
        assert!(close_enough(opus.tasks_per_200_sub, 548.0));
        assert!(close_enough(opus.weekly_budget_usd, 2281.06));
        let dotted = store
            .lookup("opus-5.5", Some("xhigh"), HarnessKind::Claude)
            .unwrap();
        assert_eq!(dotted.tasks_per_200_sub, opus.tasks_per_200_sub);
        assert_eq!(dotted.weekly_budget_usd, opus.weekly_budget_usd);
        let hinted = store
            .lookup("claude-opus-5-5[1m]", Some("xhigh"), HarnessKind::Claude)
            .unwrap();
        assert_eq!(hinted.model, opus.model);
        assert_eq!(hinted.weekly_budget_usd, opus.weekly_budget_usd);
        let bare = store
            .lookup("opus", Some("xhigh"), HarnessKind::Claude)
            .unwrap();
        assert_eq!(bare.model, "claude-opus-5");
        assert!(close_enough(bare.tasks_per_200_sub, 190.09));
        let high = store
            .lookup("opus-5.5", None, HarnessKind::Claude)
            .unwrap();
        assert_eq!(high.effort.as_deref(), Some("high"));
        assert!(close_enough(high.tasks_per_200_sub, 899.59));

        let sol = store
            .lookup("gpt-6.1-sol", Some("high"), HarnessKind::Codex)
            .unwrap();
        assert!(close_enough(sol.tasks_per_200_sub, 828.2));
        assert!(close_enough(sol.weekly_budget_usd, 887.76));
        let neon = store
            .lookup("gemini-4-neon", Some("xhigh"), HarnessKind::Antigravity)
            .unwrap();
        assert!(close_enough(neon.tasks_per_200_sub, 327.23));
        let kimi = store
            .lookup("k3.1-pro", Some("max"), HarnessKind::Kimi)
            .unwrap();
        assert_eq!(kimi.model, "kimi-k3.1-pro");
        assert!(close_enough(kimi.tasks_per_200_sub, 100.16));
        let kimi_prefixed = store
            .lookup("kimi-k3.1-pro", Some("max"), HarnessKind::Kimi)
            .unwrap();
        assert_eq!(kimi.tasks_per_200_sub, kimi_prefixed.tasks_per_200_sub);
        let kimi_native = store
            .lookup("kimi-code/k3.1-pro", Some("max"), HarnessKind::Kimi)
            .unwrap();
        assert_eq!(kimi_native.model, "kimi-k3.1-pro");
        assert_eq!(kimi.tasks_per_200_sub, kimi_native.tasks_per_200_sub);
        let grok = store
            .lookup("grok-4.7", Some("high"), HarnessKind::Grok)
            .unwrap();
        assert!(close_enough(grok.tasks_per_200_sub, 236.65));

        let dsh = store
            .lookup("deepseek-flash", Some("max"), HarnessKind::Dsh)
            .unwrap();
        assert_eq!(dsh.harness.as_deref(), Some("dsh"));
        assert!(close_enough(dsh.tasks_per_200_api, 438.78));
        assert!(
            store
                .lookup("deepseek-flash", Some("max"), HarnessKind::Claude)
                .is_none(),
            "a dsh-only row is not a Claude row"
        );

        let mean = store
            .lookup("kimi-k3", None, HarnessKind::Kimi)
            .unwrap();
        assert!(mean.geometric_mean, "kimi-k3 is ranked only at max");
        assert_eq!(mean.basis, "geometric-mean");
        assert!(close_enough(mean.tasks_per_200_sub, 53.22));
    }

    fn sentinel_snapshot(tasks: f64) -> PowerRankingSnapshot {
        PowerRankingSnapshot {
            schema_version: 1,
            as_of: Some("2026-11-19".into()),
            source_commit: Some("test".into()),
            entries: vec![PowerRankingEntry {
                config: "sentinel-model/high".into(),
                model: "sentinel-model".into(),
                effort: Some("high".into()),
                harness: None,
                tasks_per_200_sub: Some(tasks),
                tasks_per_200_api: Some(1.0),
                weekly_budget_usd: None,
                plan_usd_per_month: None,
                plan_assumed: false,
                basis: "test".into(),
                minutes: Some(1.0),
            }],
        }
    }

    fn published_body(tasks: f64) -> String {
        serde_json::to_string(&sentinel_snapshot(tasks)).unwrap()
    }

    fn write_cache_file(dir: &Path, version: &str, fetched_at: u64, snapshot: PowerRankingSnapshot) {
        let cache = PowerRankingCacheFile {
            mj_version: version.to_owned(),
            fetched_at_epoch_seconds: fetched_at,
            snapshot,
        };
        std::fs::write(
            dir.join("power-ranking-cache.json"),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
    }

    fn spawn_ranking_server(
        hits: Arc<AtomicUsize>,
        body: Arc<Mutex<(u16, String)>>,
    ) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0_u8; 2048];
                let _ = stream.read(&mut buf);
                hits.fetch_add(1, Ordering::SeqCst);
                let (status, payload) = body.lock().unwrap().clone();
                let reason = if status == 200 { "OK" } else { "ERR" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}/power-ranking.json")
    }
}
