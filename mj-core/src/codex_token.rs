//! Pre-spawn freshness gate for codex's ChatGPT OAuth token.
//!
//! The same rotation race that [`crate::claude_token`] guards against
//! for Claude Code: codex stores one refresh token in
//! `$CODEX_HOME/auth.json`, rotates it on every refresh, and refreshes
//! proactively whenever an authenticated call runs with less than five
//! minutes left on the access-token JWT (or when `last_refresh` is more
//! than eight days old). N concurrent codex processes therefore cross
//! the window together; the first refresh consumes the stored refresh
//! token and the authority answers every later attempt with
//! `refresh_token_reused`, which codex treats as terminal ("Please log
//! out and sign in again"). Callers gate here before spawning a codex
//! process: the machine-wide lease winner asks one `codex app-server`
//! to refresh, codex persists the rotated token set, and every waiter
//! then spawns against the rewritten `auth.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;

use crate::usage_fact::{StoredFact, UsageFactStore};

/// Refresh when the access token has this little life left. Codex's
/// own proactive window is five minutes; staying inside it means the
/// gate always rotates before any codex process would try on its own.
const REFRESH_WINDOW_MS: i64 = 4 * 60 * 1000;

/// When the access token is not a parseable JWT, codex falls back to
/// refreshing once `last_refresh` is older than eight days. Rotate just
/// ahead of that edge too.
const LAST_REFRESH_INTERVAL_MS: i64 = 8 * 24 * 60 * 60 * 1000;

/// Provider key of the machine-wide refresh fact.
const SHARED_FACT_PROVIDER: &str = "codex-token-refresh";
/// A refresh outcome published this recently satisfies the gate without
/// another probe — the retry cooldown when a refresh is impossible
/// (e.g. genuinely signed out), so spawn storms cost one probe per TTL.
const SHARED_FACT_TTL: Duration = Duration::from_secs(60);
/// Checkout lease on the shared fact: the refresh timeout plus slack,
/// short enough that a crashed holder does not block others for long.
const CHECKOUT_LEASE: Duration =
    crate::codex_usage::TOKEN_REFRESH_TIMEOUT.saturating_add(Duration::from_secs(10));
const CHECKOUT_POLL: Duration = Duration::from_millis(500);

/// Whether a to-be-spawned agent invocation is codex. The roster
/// adapter id is authoritative when the caller has one; spawn paths
/// that only know the command line identify the vendor by the bundled
/// adapter package name.
pub fn is_codex_invocation(adapter_source_id: Option<&str>, args: &[String]) -> bool {
    if adapter_source_id.is_some_and(|id| id == "codex-acp") {
        return true;
    }
    args.iter().any(|arg| arg.contains("codex-acp"))
}

/// Rotate a near-expiry codex OAuth token before spawning a codex
/// process. Best effort: every failure path degrades to spawning with
/// the current credentials, exactly as before the gate existed.
pub async fn ensure_fresh_before_spawn(cwd: PathBuf, env: &HashMap<String, String>) {
    if !needs_refresh(env, now_ms()) {
        return;
    }
    tracing::info!("codex OAuth token near expiry; refreshing via shared app-server probe");
    let store = UsageFactStore::new(crate::usage_fact::default_store_path());
    let env_for_probe = env.clone();
    let outcome = refresh_once_shared(store, SHARED_FACT_TTL, move || async move {
        crate::codex_usage::force_token_refresh(cwd, env_for_probe).await
    })
    .await;
    if let Some(Err(reason)) = outcome {
        tracing::debug!("codex token refresh probe: {reason}");
    }
    if needs_refresh(env, now_ms()) {
        tracing::warn!(
            "codex OAuth token still near expiry after a refresh probe; sign-in may be required"
        );
    }
}

/// How long a long-running steward should wait before its next
/// proactive freshness check: just as the token enters the refresh
/// window, so one steward pass per token lifetime keeps `auth.json`
/// fresh for every process on the machine. Falls back to an idle
/// recheck when there is nothing to steward.
pub fn steward_delay(env: &HashMap<String, String>) -> Duration {
    const IDLE_RECHECK: Duration = Duration::from_secs(15 * 60);
    let Some(state) = token_state(env) else {
        return IDLE_RECHECK;
    };
    let delay_ms = state
        .refresh_due_at_ms()
        .saturating_sub(now_ms())
        .saturating_add(1_000)
        .max(0);
    Duration::from_millis(delay_ms as u64)
}

fn needs_refresh(env: &HashMap<String, String>, now_ms: i64) -> bool {
    token_state(env).is_some_and(|state| state.refresh_due_at_ms() <= now_ms)
}

/// The two clocks codex consults before refreshing, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenState {
    access_token_expires_at_ms: Option<i64>,
    last_refresh_ms: Option<i64>,
}

impl TokenState {
    /// The instant at which codex would refresh on its own, minus the
    /// gate's margin. Codex consults the JWT expiry when the token
    /// parses and falls back to the `last_refresh` age only otherwise;
    /// the gate follows the same precedence so it never rotates on a
    /// schedule codex itself would not.
    fn refresh_due_at_ms(self) -> i64 {
        if let Some(expires_at) = self.access_token_expires_at_ms {
            return expires_at.saturating_sub(REFRESH_WINDOW_MS);
        }
        self.last_refresh_ms.map_or(i64::MAX, |last_refresh| {
            last_refresh
                .saturating_add(LAST_REFRESH_INTERVAL_MS)
                .saturating_sub(REFRESH_WINDOW_MS)
        })
    }
}

/// `None` when there is no rotating credential to protect: API-key
/// logins, no `auth.json`, or a file this gate cannot interpret.
fn token_state(env: &HashMap<String, String>) -> Option<TokenState> {
    if env_defined(env, "OPENAI_API_KEY") {
        return None;
    }
    let contents = std::fs::read(auth_path(env)?).ok()?;
    token_state_from_auth_json(&contents)
}

fn token_state_from_auth_json(contents: &[u8]) -> Option<TokenState> {
    let document: serde_json::Value = serde_json::from_slice(contents).ok()?;
    if document
        .get("auth_mode")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|mode| mode != "chatgpt")
    {
        return None;
    }
    let tokens = document.get("tokens")?;
    tokens
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())?;
    let access_token_expires_at_ms = tokens
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .and_then(jwt_expires_at_ms);
    let last_refresh_ms = document
        .get("last_refresh")
        .and_then(serde_json::Value::as_str)
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
        .map(|stamp| stamp.timestamp_millis());
    if access_token_expires_at_ms.is_none() && last_refresh_ms.is_none() {
        return None;
    }
    Some(TokenState {
        access_token_expires_at_ms,
        last_refresh_ms,
    })
}

/// The `exp` claim of an unverified JWT, as codex reads it for its own
/// proactive refresh decision.
fn jwt_expires_at_ms(jwt: &str) -> Option<i64> {
    let payload = jwt.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims.get("exp")?;
    exp.as_i64()
        .or_else(|| exp.as_f64().map(|value| value as i64))
        .map(|seconds| seconds.saturating_mul(1000))
}

fn auth_path(env: &HashMap<String, String>) -> Option<PathBuf> {
    let root = env_value(env, "CODEX_HOME")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))?;
    Some(root.join("auth.json"))
}

/// Spawn-env overrides take precedence over the process environment,
/// mirroring what the spawned codex process itself will see.
fn env_value(env: &HashMap<String, String>, name: &str) -> Option<String> {
    env.get(name).cloned().or_else(|| std::env::var(name).ok())
}

fn env_defined(env: &HashMap<String, String>, name: &str) -> bool {
    env_value(env, name).is_some_and(|value| !value.trim().is_empty())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Run `probe` once machine-wide: the caller that wins the checkout
/// lease runs it and publishes the outcome, everyone else waits for
/// that publication. A fact younger than `max_age` at call start is
/// accepted without probing. `None` when no outcome became available
/// within a full lease (storage trouble, or a holder that never
/// published).
async fn refresh_once_shared<F, Fut>(
    store: UsageFactStore,
    max_age: Duration,
    probe: F,
) -> Option<Result<(), String>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    static OWNER_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let started = crate::usage_fact::unix_now();
    let owner = format!(
        "mj-{}-{}",
        std::process::id(),
        OWNER_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let deadline = Instant::now() + CHECKOUT_LEASE;
    loop {
        if let Some(fact) = read_fact(&store)
            .await
            .filter(|fact| fact_is_current(fact, started, max_age))
        {
            return Some(decode_fact(&fact));
        }
        if checkout_fact(&store, &owner).await {
            let result = probe().await;
            let payload =
                serde_json::json!({ "ok": result.is_ok(), "error": result.as_ref().err() });
            publish_fact(&store, payload.to_string(), owner).await;
            return Some(result);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(CHECKOUT_POLL).await;
    }
}

fn fact_is_current(fact: &StoredFact, started: i64, max_age: Duration) -> bool {
    let max_age = i64::try_from(max_age.as_secs()).unwrap_or(i64::MAX);
    fact.fetched_at >= started || started.saturating_sub(fact.fetched_at) <= max_age
}

fn decode_fact(fact: &StoredFact) -> Result<(), String> {
    let document: serde_json::Value = serde_json::from_str(&fact.payload).unwrap_or_default();
    if document.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(());
    }
    Err(document
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("earlier refresh probe failed")
        .to_string())
}

async fn read_fact(store: &UsageFactStore) -> Option<StoredFact> {
    let store = store.clone();
    match tokio::task::spawn_blocking(move || store.read(SHARED_FACT_PROVIDER)).await {
        Ok(Ok(fact)) => fact,
        Ok(Err(error)) => {
            tracing::debug!("read shared codex refresh fact: {error}");
            None
        }
        Err(_) => None,
    }
}

/// A storage failure counts as a successful checkout: the shared lease
/// must never make spawning worse than probing directly.
async fn checkout_fact(store: &UsageFactStore, owner: &str) -> bool {
    let store = store.clone();
    let owner = owner.to_string();
    let now = crate::usage_fact::unix_now();
    match tokio::task::spawn_blocking(move || {
        store.try_checkout(SHARED_FACT_PROVIDER, &owner, CHECKOUT_LEASE, now)
    })
    .await
    {
        Ok(Ok(acquired)) => acquired,
        Ok(Err(error)) => {
            tracing::debug!("checkout shared codex refresh fact: {error}");
            true
        }
        Err(_) => true,
    }
}

async fn publish_fact(store: &UsageFactStore, payload: String, owner: String) {
    let store = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        store.publish(
            SHARED_FACT_PROVIDER,
            &payload,
            &owner,
            crate::usage_fact::unix_now(),
        )
    })
    .await;
    if let Ok(Err(error)) = result {
        tracing::debug!("publish shared codex refresh fact: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn jwt_with_exp(exp_seconds: i64) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            engine.encode(r#"{"alg":"RS256"}"#),
            engine.encode(format!(r#"{{"exp":{exp_seconds},"sub":"u"}}"#)),
        )
    }

    fn auth_json(access_exp_seconds: Option<i64>, last_refresh: Option<&str>) -> Vec<u8> {
        let mut tokens = serde_json::json!({
            "id_token": "x.y.z",
            "refresh_token": "refresh",
            "account_id": "acct",
        });
        if let Some(exp) = access_exp_seconds {
            tokens["access_token"] = serde_json::Value::String(jwt_with_exp(exp));
        }
        let mut document = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": tokens,
        });
        if let Some(stamp) = last_refresh {
            document["last_refresh"] = serde_json::Value::String(stamp.to_string());
        }
        document.to_string().into_bytes()
    }

    fn env_with_codex_home(dir: &std::path::Path) -> HashMap<String, String> {
        HashMap::from([("CODEX_HOME".to_string(), dir.to_string_lossy().into_owned())])
    }

    #[test]
    fn detects_codex_invocations_by_source_id_and_args() {
        assert!(is_codex_invocation(Some("codex-acp"), &[]));
        assert!(!is_codex_invocation(Some("claude-acp"), &[]));
        assert!(is_codex_invocation(
            None,
            &[
                "-y".to_string(),
                "@agentclientprotocol/codex-acp".to_string(),
            ],
        ));
        assert!(!is_codex_invocation(
            None,
            &[
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp".to_string(),
            ],
        ));
    }

    #[test]
    fn reads_the_access_token_expiry_from_the_jwt() {
        let state =
            token_state_from_auth_json(&auth_json(Some(1_700_000_000), None)).expect("state");
        assert_eq!(state.access_token_expires_at_ms, Some(1_700_000_000_000));
        assert_eq!(state.last_refresh_ms, None);

        let state = token_state_from_auth_json(&auth_json(
            Some(1_700_000_000),
            Some("2026-08-21T06:42:59.385330Z"),
        ))
        .expect("state");
        let last_refresh = state.last_refresh_ms.expect("last refresh");
        assert_eq!(last_refresh / 1000, 1_787_294_579);
    }

    #[test]
    fn api_key_logins_and_unusable_files_skip_the_gate() {
        assert_eq!(token_state_from_auth_json(b"not json"), None);
        assert_eq!(
            token_state_from_auth_json(
                br#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-x","tokens":null}"#
            ),
            None
        );
        // A chatgpt record without a refresh token has nothing to rotate.
        assert_eq!(
            token_state_from_auth_json(
                br#"{"auth_mode":"chatgpt","tokens":{"access_token":"a.b.c","refresh_token":""}}"#
            ),
            None
        );
        // Unparseable JWT and no last_refresh: nothing to schedule on.
        assert_eq!(
            token_state_from_auth_json(
                br#"{"auth_mode":"chatgpt","tokens":{"access_token":"garbage","refresh_token":"r"}}"#
            ),
            None
        );

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("auth.json"), auth_json(Some(0), None)).expect("write");
        let mut env = env_with_codex_home(dir.path());
        env.insert("OPENAI_API_KEY".to_string(), "sk-test".to_string());
        assert!(!needs_refresh(&env, 1_000_000));
        env.insert("OPENAI_API_KEY".to_string(), "  ".to_string());
        assert!(needs_refresh(&env, 1_000_000));
    }

    #[test]
    fn refresh_triggers_inside_the_jwt_window_and_after_expiry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_with_codex_home(dir.path());
        let now = 1_700_000_000_000;
        let write = |exp_ms: i64| {
            std::fs::write(
                dir.path().join("auth.json"),
                auth_json(Some(exp_ms / 1000), None),
            )
            .expect("write");
        };

        write(now + REFRESH_WINDOW_MS + 1_000);
        assert!(!needs_refresh(&env, now));
        write(now + REFRESH_WINDOW_MS);
        assert!(needs_refresh(&env, now));
        write(now - 24 * 60 * 60 * 1000);
        assert!(needs_refresh(&env, now));
    }

    #[test]
    fn the_jwt_clock_wins_and_last_refresh_is_only_a_fallback() {
        let ten_days_out = 1_700_000_000_000 + 10 * 24 * 60 * 60 * 1000;
        // Codex ignores last_refresh while the JWT parses, so a 10-day
        // token is not rotated at the 8-day mark.
        let with_both = TokenState {
            access_token_expires_at_ms: Some(ten_days_out),
            last_refresh_ms: Some(1_700_000_000_000),
        };
        assert_eq!(
            with_both.refresh_due_at_ms(),
            ten_days_out - REFRESH_WINDOW_MS
        );

        // No parseable JWT: the 8-day last_refresh rule applies.
        let last_refresh_only = TokenState {
            access_token_expires_at_ms: None,
            last_refresh_ms: Some(1_700_000_000_000),
        };
        assert_eq!(
            last_refresh_only.refresh_due_at_ms(),
            1_700_000_000_000 + LAST_REFRESH_INTERVAL_MS - REFRESH_WINDOW_MS
        );
    }

    #[test]
    fn steward_sleeps_to_the_window_edge_and_rechecks_idle_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_with_codex_home(dir.path());
        assert_eq!(steward_delay(&env), Duration::from_secs(15 * 60));

        let in_two_hours = (now_ms() + 2 * 60 * 60 * 1000) / 1000;
        std::fs::write(
            dir.path().join("auth.json"),
            auth_json(Some(in_two_hours), None),
        )
        .expect("write");
        let delay = steward_delay(&env);
        assert!(delay > Duration::from_secs(60 * 60), "{delay:?}");
        assert!(delay < Duration::from_secs(2 * 60 * 60), "{delay:?}");

        std::fs::write(
            dir.path().join("auth.json"),
            auth_json(Some(now_ms() / 1000 - 1), None),
        )
        .expect("write");
        assert_eq!(steward_delay(&env), Duration::ZERO);
    }

    #[tokio::test]
    async fn concurrent_gates_share_one_probe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = UsageFactStore::new(dir.path().join("usage.sqlite3"));
        let probes = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = store.clone();
            let probes = probes.clone();
            handles.push(tokio::spawn(async move {
                refresh_once_shared(store, SHARED_FACT_TTL, move || async move {
                    probes.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(())
                })
                .await
            }));
        }
        for handle in handles {
            assert_eq!(handle.await.expect("join"), Some(Ok(())));
        }
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_recent_failure_is_the_cooldown_and_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = UsageFactStore::new(dir.path().join("usage.sqlite3"));
        let first = refresh_once_shared(store.clone(), SHARED_FACT_TTL, || async {
            Err("not signed in".to_string())
        })
        .await;
        assert_eq!(first, Some(Err("not signed in".to_string())));

        let second = refresh_once_shared(store, SHARED_FACT_TTL, || async {
            panic!("must not probe inside the cooldown")
        })
        .await;
        assert_eq!(second, Some(Err("not signed in".to_string())));
    }
}
