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
//!
//! Claude Code keeps the credential in `$CLAUDE_CONFIG_DIR/.credentials.json`
//! on Linux and Windows, but on macOS the default profile lives in the
//! login Keychain (service `Claude Code-credentials`) and no file exists.
//! The gate reads whichever store the spawned process will use; without
//! the Keychain fallback every macOS host silently ran unprotected.

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
/// paths (side sessions and probes) only know the command line,
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
    let Some(expires_at) = oauth_expires_at_ms(env) else {
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
    let Some(expires_at) = oauth_expires_at_ms(env) else {
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

/// Expiry of the OAuth access token the spawned process will load:
/// the credential file when present, otherwise the macOS Keychain.
fn oauth_expires_at_ms(env: &HashMap<String, String>) -> Option<i64> {
    let path = credentials_path(env)?;
    expires_at_from_store(
        &path,
        uses_default_config_dir(env),
        read_keychain_credentials,
    )
}

fn expires_at_from_store(
    path: &std::path::Path,
    default_profile: bool,
    keychain: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<i64> {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        // Only a missing file means "look elsewhere"; an unreadable or
        // malformed file is a real answer (skip the gate), not a cue to
        // consult a store the spawned process will not use.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !default_profile {
                return None;
            }
            keychain()?
        }
        Err(_) => return None,
    };
    expires_at_from_credentials(&contents)
}

fn expires_at_from_credentials(contents: &[u8]) -> Option<i64> {
    let document: serde_json::Value = serde_json::from_slice(contents).ok()?;
    let expires_at = document.pointer("/claudeAiOauth/expiresAt")?;
    expires_at
        .as_i64()
        .or_else(|| expires_at.as_f64().map(|value| value as i64))
}

/// Claude Code names the Keychain item for the default profile
/// `Claude Code-credentials`; custom `CLAUDE_CONFIG_DIR` profiles use a
/// name derived from the directory, which the gate does not reproduce
/// — those profiles keep the file-only behavior.
fn uses_default_config_dir(env: &HashMap<String, String>) -> bool {
    let Some(configured) = env_value(env, "CLAUDE_CONFIG_DIR") else {
        return true;
    };
    let configured = configured.trim();
    if configured.is_empty() {
        return true;
    }
    dirs::home_dir().is_some_and(|home| std::path::Path::new(configured) == home.join(".claude"))
}

/// Claude Code's macOS credential store. Claude Code itself writes and
/// reads the item through the `security` tool, so that tool is already
/// on the item's access list and this read raises no Keychain prompt.
#[cfg(target_os = "macos")]
fn read_keychain_credentials() -> Option<Vec<u8>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[cfg(not(target_os = "macos"))]
fn read_keychain_credentials() -> Option<Vec<u8>> {
    None
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

    fn keychain_blob(expires_at: i64) -> Vec<u8> {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "token",
                "refreshToken": "refresh",
                "expiresAt": expires_at,
            }
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn missing_file_on_the_default_profile_falls_back_to_the_keychain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".credentials.json");
        // macOS default profile: no file, Keychain answers.
        assert_eq!(
            expires_at_from_store(&path, true, || Some(keychain_blob(42))),
            Some(42)
        );
        // Keychain item absent (signed out, or not macOS): gate skips.
        assert_eq!(expires_at_from_store(&path, true, || None), None);
        // Keychain returns garbage: gate skips rather than guessing.
        assert_eq!(
            expires_at_from_store(&path, true, || Some(b"not json".to_vec())),
            None
        );
    }

    #[test]
    fn custom_profiles_never_consult_the_keychain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".credentials.json");
        assert_eq!(
            expires_at_from_store(&path, false, || panic!("keychain must not be read")),
            None
        );
    }

    #[test]
    fn an_existing_file_wins_over_the_keychain() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_credentials(dir.path(), serde_json::json!(7));
        let path = dir.path().join(".credentials.json");
        assert_eq!(
            expires_at_from_store(&path, true, || panic!("keychain must not be read")),
            Some(7)
        );
        // A present-but-malformed file is a real answer, not a fallback cue.
        std::fs::write(&path, "not json").expect("write");
        assert_eq!(
            expires_at_from_store(&path, true, || panic!("keychain must not be read")),
            None
        );
    }

    #[test]
    fn default_profile_detection_follows_claude_config_dir() {
        assert!(uses_default_config_dir(&HashMap::new()));
        assert!(uses_default_config_dir(&HashMap::from([(
            "CLAUDE_CONFIG_DIR".to_string(),
            "  ".to_string()
        )])));
        let home_profile = dirs::home_dir().expect("home").join(".claude");
        assert!(uses_default_config_dir(&HashMap::from([(
            "CLAUDE_CONFIG_DIR".to_string(),
            home_profile.to_string_lossy().into_owned()
        )])));
        assert!(!uses_default_config_dir(&HashMap::from([(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/somewhere/else".to_string()
        )])));
    }

    /// Live check against this machine's Keychain; run by hand on a
    /// signed-in macOS host with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn live_keychain_holds_a_claude_expiry() {
        let expires_at = read_keychain_credentials()
            .as_deref()
            .and_then(expires_at_from_credentials)
            .expect("Claude Code-credentials item with claudeAiOauth.expiresAt");
        assert!(expires_at > 0, "{expires_at}");
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
