//! Pre-spawn freshness gate for Claude's OAuth access token.
//!
//! Claude Code rotates its refresh token on every refresh, so N
//! concurrent claude processes crossing the access token's expiry
//! together race each other: the first refresh consumes the stored
//! refresh token and every process still holding the old one is signed
//! out ("OAuth session expired and could not be refreshed"). Claude
//! Code refreshes whenever an authenticated call runs with less than
//! five minutes left before `expiresAt`, so funneling one probe
//! through the machine-wide usage lease inside that margin rotates the
//! token once for everyone: callers gate here before spawning a Claude
//! process, the lease winner's `/usage` probe performs the rotation,
//! and every waiter then spawns against the rewritten credential file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Refresh when the access token has this little life left, in
/// milliseconds. Claude Code's own refresh margin is five minutes;
/// this window plus the shared usage fact's 60s TTL must stay inside
/// it so that whenever the gate accepts a cached fact instead of
/// probing, the probe that produced the fact already ran inside
/// Claude's margin and therefore already rotated the token.
const REFRESH_WINDOW_MS: i64 = 4 * 60 * 1000;

/// Whether a to-be-spawned agent invocation is Claude Code. The roster
/// adapter id is authoritative when the caller has one; several spawn
/// paths (side sessions, ragnarok, probes) only know the command line,
/// where the bundled adapter package name identifies the vendor.
pub fn is_claude_invocation(adapter_source_id: Option<&str>, args: &[String]) -> bool {
    if adapter_source_id.is_some_and(|id| id == "claude-acp") {
        return true;
    }
    args.iter().any(|arg| arg.contains("claude-agent-acp"))
}

/// Rotate a near-expiry Claude OAuth token before spawning a Claude
/// process. Best effort: every failure path degrades to spawning with
/// the current credentials, exactly as before the gate existed. The
/// shared usage fact's TTL doubles as the retry cooldown when a
/// refresh is impossible (e.g. genuinely signed out), so stale-token
/// spawn storms cost at most one probe per TTL machine-wide.
pub async fn ensure_fresh_before_spawn(cwd: PathBuf, env: &HashMap<String, String>) {
    if !needs_refresh(env, now_ms()) {
        return;
    }
    tracing::info!("claude OAuth token near expiry; refreshing via shared usage probe");
    if let Err(error) = crate::claude_usage::query(cwd.clone(), env.clone()).await {
        tracing::debug!("pre-spawn claude token refresh probe: {error}");
        return;
    }
    if !needs_refresh(env, now_ms()) {
        return;
    }
    // The shared usage fact is machine-wide while credentials are per
    // CLAUDE_CONFIG_DIR, so a fact probed under another profile — or a
    // probe that failed to rotate for any reason — proves nothing
    // about this token. Force one serialized probe with this profile's
    // environment, rate-limited in-process so stale-token spawn storms
    // cannot bypass the shared TTL cooldown.
    if !forced_probe_allowed(now_ms()) {
        return;
    }
    if let Err(error) = crate::claude_usage::query_fresh(cwd, env.clone()).await {
        tracing::debug!("forced claude token refresh probe: {error}");
        return;
    }
    if needs_refresh(env, now_ms()) {
        tracing::warn!(
            "claude OAuth token still near expiry after a refresh probe; sign-in may be required"
        );
    }
}

/// Minimum spacing between forced (TTL-bypassing) refresh probes from
/// this process. Matches the shared fact's TTL so escalation can never
/// probe faster than the ordinary path would.
const FORCED_PROBE_COOLDOWN_MS: i64 = 60_000;

fn forced_probe_allowed(now_ms: i64) -> bool {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST_FORCED_MS: AtomicI64 = AtomicI64::new(0);
    let last = LAST_FORCED_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last) < FORCED_PROBE_COOLDOWN_MS {
        return false;
    }
    LAST_FORCED_MS
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// How long a long-running steward should wait before its next
/// proactive freshness check: just as the token enters the refresh
/// window, so one steward pass per token lifetime keeps the credential
/// file fresh for every process on the machine — including seats that
/// are already running and would otherwise meet an expired token
/// mid-turn. Falls back to an idle recheck when there is nothing to
/// steward (env credentials, no credential file).
pub fn steward_delay(env: &HashMap<String, String>) -> std::time::Duration {
    const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    if env_defined(env, "CLAUDE_CODE_OAUTH_TOKEN") || env_defined(env, "ANTHROPIC_API_KEY") {
        return IDLE_RECHECK;
    }
    let Some(expires_at) = credentials_path(env).and_then(oauth_expires_at_ms) else {
        return IDLE_RECHECK;
    };
    let delay_ms = expires_at
        .saturating_sub(now_ms())
        .saturating_sub(REFRESH_WINDOW_MS)
        .saturating_add(1_000)
        .max(0);
    std::time::Duration::from_millis(delay_ms as u64)
}

fn needs_refresh(env: &HashMap<String, String>, now_ms: i64) -> bool {
    // Env-token and API-key logins have no refresh dance to protect.
    if env_defined(env, "CLAUDE_CODE_OAUTH_TOKEN") || env_defined(env, "ANTHROPIC_API_KEY") {
        return false;
    }
    let Some(expires_at) = credentials_path(env).and_then(oauth_expires_at_ms) else {
        return false;
    };
    expires_at.saturating_sub(now_ms) <= REFRESH_WINDOW_MS
}

/// Spawn-env overrides take precedence over the process environment,
/// mirroring what the spawned claude process itself will see.
fn env_value(env: &HashMap<String, String>, name: &str) -> Option<String> {
    env.get(name).cloned().or_else(|| std::env::var(name).ok())
}

fn env_defined(env: &HashMap<String, String>, name: &str) -> bool {
    env_value(env, name).is_some_and(|value| !value.trim().is_empty())
}

fn credentials_path(env: &HashMap<String, String>) -> Option<PathBuf> {
    let root = env_value(env, "CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))?;
    Some(root.join(".credentials.json"))
}

fn oauth_expires_at_ms(path: PathBuf) -> Option<i64> {
    let contents = std::fs::read(path).ok()?;
    let document: serde_json::Value = serde_json::from_slice(&contents).ok()?;
    let expires_at = document.pointer("/claudeAiOauth/expiresAt")?;
    expires_at
        .as_i64()
        .or_else(|| expires_at.as_f64().map(|value| value as i64))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with_config_dir(dir: &std::path::Path) -> HashMap<String, String> {
        HashMap::from([(
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        )])
    }

    fn write_credentials(dir: &std::path::Path, expires_at: serde_json::Value) {
        std::fs::write(
            dir.join(".credentials.json"),
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "token",
                    "refreshToken": "refresh",
                    "expiresAt": expires_at,
                }
            })
            .to_string(),
        )
        .expect("write credentials");
    }

    #[test]
    fn detects_claude_invocations_by_source_id_and_args() {
        assert!(is_claude_invocation(Some("claude-acp"), &[]));
        assert!(!is_claude_invocation(Some("codex-acp"), &[]));
        assert!(is_claude_invocation(
            None,
            &[
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp".to_string(),
            ],
        ));
        assert!(!is_claude_invocation(
            None,
            &[
                "-y".to_string(),
                "@agentclientprotocol/codex-acp".to_string()
            ],
        ));
    }

    #[test]
    fn static_credentials_never_need_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_credentials(dir.path(), serde_json::json!(0));
        let mut env = env_with_config_dir(dir.path());
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "token".to_string());
        assert!(!needs_refresh(&env, 1_000_000));

        let mut env = env_with_config_dir(dir.path());
        env.insert("ANTHROPIC_API_KEY".to_string(), "key".to_string());
        assert!(!needs_refresh(&env, 1_000_000));

        // Whitespace-only values do not count as a configured credential.
        let mut env = env_with_config_dir(dir.path());
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "  ".to_string());
        assert!(needs_refresh(&env, 1_000_000));
    }

    #[test]
    fn missing_or_malformed_credentials_skip_the_gate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_with_config_dir(dir.path());
        assert!(!needs_refresh(&env, 1_000_000));

        std::fs::write(dir.path().join(".credentials.json"), "not json").expect("write");
        assert!(!needs_refresh(&env, 1_000_000));

        write_credentials(dir.path(), serde_json::json!("soon"));
        assert!(!needs_refresh(&env, 1_000_000));
    }

    #[test]
    fn forced_probes_are_rate_limited_in_process() {
        // The static cooldown is process-wide, so this test owns a
        // distant time range other tests never touch.
        let base = 9_000_000_000_000;
        assert!(forced_probe_allowed(base));
        assert!(!forced_probe_allowed(base + FORCED_PROBE_COOLDOWN_MS - 1));
        assert!(forced_probe_allowed(base + FORCED_PROBE_COOLDOWN_MS));
    }

    #[test]
    fn steward_sleeps_to_the_window_edge_and_rechecks_idle_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_with_config_dir(dir.path());

        // No credential file: idle recheck.
        assert_eq!(steward_delay(&env), std::time::Duration::from_secs(15 * 60));

        // Static credentials: idle recheck regardless of the file.
        write_credentials(dir.path(), serde_json::json!(0));
        let mut static_env = env_with_config_dir(dir.path());
        static_env.insert("ANTHROPIC_API_KEY".to_string(), "key".to_string());
        assert_eq!(
            steward_delay(&static_env),
            std::time::Duration::from_secs(15 * 60)
        );

        // A token with hours of life sleeps until the window opens.
        let eight_hours = 8 * 60 * 60 * 1000;
        write_credentials(dir.path(), serde_json::json!(now_ms() + eight_hours));
        let delay = steward_delay(&env);
        assert!(
            delay > std::time::Duration::from_secs(7 * 60 * 60),
            "{delay:?}"
        );
        assert!(
            delay < std::time::Duration::from_secs(8 * 60 * 60),
            "{delay:?}"
        );

        // Inside the window (or expired) the steward fires immediately.
        write_credentials(dir.path(), serde_json::json!(now_ms() - 1_000));
        assert_eq!(steward_delay(&env), std::time::Duration::ZERO);
    }

    #[test]
    fn refresh_triggers_inside_the_window_and_after_expiry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env_with_config_dir(dir.path());
        let now = 1_700_000_000_000;

        write_credentials(dir.path(), serde_json::json!(now + REFRESH_WINDOW_MS + 1));
        assert!(!needs_refresh(&env, now));

        write_credentials(dir.path(), serde_json::json!(now + REFRESH_WINDOW_MS));
        assert!(needs_refresh(&env, now));

        write_credentials(dir.path(), serde_json::json!(now - 8 * 60 * 60 * 1000));
        assert!(needs_refresh(&env, now));

        // Fractional timestamps (written by other tooling) still parse.
        write_credentials(dir.path(), serde_json::json!((now - 1) as f64));
        assert!(needs_refresh(&env, now));
    }
}
