//! Claude Code quota polling, OAuth renewal, and `/usage` parsing.
//!
//! The Claude ACP agent exposes token usage over ACP, but the subscription
//! quota shown by Claude Code lives behind its local `/usage` command.  Keep
//! this module independent from the UI state machine so the parser can be
//! tested against captured command output without spawning `claude`.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::targets::{CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec};

const USAGE_TIMEOUT: Duration = Duration::from_secs(20);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(10);
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
// The reset-grant endpoint uses the CLI surface to determine eligibility.
const USAGE_USER_AGENT: &str = "claude-cli/2.1.283 (external, cli)";

/// Quota-row copy when the stored Claude OAuth access token is past `expiresAt`.
pub(crate) const LOGIN_EXPIRED: &str = "login expired";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageReport {
    pub banked_resets: Option<u64>,
    pub five_hour: Option<ClaudeUsageWindow>,
    pub week: Option<ClaudeUsageWindow>,
    pub scoped_weeks: Vec<(String, ClaudeUsageWindow)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageWindow {
    pub remaining_percent: u8,
    pub duration_seconds: Option<i64>,
    /// Text following `reset` in Claude Code output, without the word itself.
    pub reset_context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeUsageError {
    TimedOut,
    NotSignedIn,
    LoginExpired,
    Refresh(String),
    Query(String),
    Parse,
    /// The endpoint answered 429. The delay is what `Retry-After` asked for,
    /// when it said.
    RateLimited(Option<Duration>),
}

impl fmt::Display for ClaudeUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => write!(f, "claude /usage timed out"),
            Self::NotSignedIn => write!(f, "Claude Code is not signed in"),
            Self::LoginExpired => write!(f, "{LOGIN_EXPIRED}"),
            Self::Refresh(error) => write!(f, "refresh Claude login: {error}"),
            Self::Query(error) => write!(f, "query Claude usage: {error}"),
            Self::Parse => write!(f, "could not parse claude /usage output"),
            Self::RateLimited(_) => write!(f, "rate limited"),
        }
    }
}

/// Query the same OAuth usage endpoint as Claude Code's interactive `/usage`,
/// except on macOS, where Claude Code is asked directly.
///
/// The endpoint is authenticated with the access token from the profile home's
/// `.credentials.json`. On macOS that file does not exist: Claude Code keeps
/// the credentials in the Keychain, where only Claude Code itself reads them.
/// The API has no token to send there, so its print-mode `/usage` output is
/// the only quota macOS can report.
pub async fn query(
    home: PathBuf,
    environment: HashMap<String, String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let executor = Arc::new(CancellableProcessExecutor::with_timeout(
        USAGE_TIMEOUT + REFRESH_LOCK_WAIT + REFRESH_TIMEOUT,
    ));
    let _cancel = executor.cancel_on_drop();
    if cfg!(target_os = "macos") {
        return query_cli_with(environment, executor).await;
    }
    query_with(home, environment, USAGE_URL, executor).await
}

/// Read the quota out of Claude Code's own `/usage` output.
async fn query_cli_with(
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let output = run_claude_usage(environment, executor, "read Claude usage")
        .await
        .map_err(ClaudeUsageError::Query)?;
    // Claude Code reports a missing login on stdout and exits zero, so both
    // streams are read before the status is judged.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // A report that parses is the answer. Classifying first would let the
    // free-form sections below the windows decide the result: a skill or MCP
    // server whose name happens to contain "unauthorized" is not a login
    // failure.
    if output.status == 0
        && let Some(report) = parse_cli_usage(&text)
    {
        return Ok(report);
    }
    if is_authentication_error(&text) {
        return Err(ClaudeUsageError::NotSignedIn);
    }
    if output.status != 0 {
        return Err(ClaudeUsageError::Query(format!(
            "claude /usage exited with status {}",
            output.status
        )));
    }
    Err(ClaudeUsageError::Parse)
}

async fn query_with(
    home: PathBuf,
    environment: HashMap<String, String>,
    usage_url: &str,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let client = reqwest::Client::builder()
        .timeout(USAGE_TIMEOUT)
        .build()
        .map_err(|error| ClaudeUsageError::Query(error.to_string()))?;
    let credentials = read_credentials(&home).await?;
    match oauth_access_token(&credentials, mj_core::clock::epoch_millis()) {
        Ok(token) => match query_api(&client, usage_url, token).await {
            Err(ClaudeUsageError::LoginExpired) if oauth_has_refresh_token(&credentials) => {
                refresh_and_retry(
                    &client,
                    usage_url,
                    home,
                    environment,
                    executor,
                    Some(token.to_owned()),
                )
                .await
            }
            result => result,
        },
        Err(ClaudeUsageError::LoginExpired) if oauth_has_refresh_token(&credentials) => {
            refresh_and_retry(&client, usage_url, home, environment, executor, None).await
        }
        Err(error) => Err(error),
    }
}

async fn read_credentials(home: &std::path::Path) -> Result<Value, ClaudeUsageError> {
    let credentials = tokio::fs::read(home.join(".credentials.json"))
        .await
        .map_err(|_| ClaudeUsageError::NotSignedIn)?;
    serde_json::from_slice(&credentials).map_err(|_| ClaudeUsageError::NotSignedIn)
}

async fn query_api(
    client: &reqwest::Client,
    usage_url: &str,
    token: &str,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let response = client
        .get(usage_url)
        .query(&[("cedar_ember", "1"), ("skip_spend", "1")])
        .header(reqwest::header::USER_AGENT, USAGE_USER_AGENT)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                ClaudeUsageError::TimedOut
            } else {
                ClaudeUsageError::Query(error.to_string())
            }
        })?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(ClaudeUsageError::LoginExpired);
    }
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(ClaudeUsageError::RateLimited(
            crate::quota::retry_after_header(response.headers()),
        ));
    }
    if !response.status().is_success() {
        return Err(ClaudeUsageError::Query(format!(
            "HTTP {}",
            response.status()
        )));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|error| ClaudeUsageError::Query(error.to_string()))?;
    parse_api_usage(&payload).ok_or(ClaudeUsageError::Parse)
}

async fn refresh_and_retry(
    client: &reqwest::Client,
    usage_url: &str,
    home: PathBuf,
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    rejected_token: Option<String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    // Claude 2.1.312 takes these two proper-lockfile directory locks during
    // its own refresh. auth login's explicit exchange does not take them.
    let canonical_home = tokio::fs::canonicalize(&home)
        .await
        .map_err(|error| ClaudeUsageError::Refresh(error.to_string()))?;
    let mut legacy_path = canonical_home.as_os_str().to_os_string();
    legacy_path.push(".lock");
    let mut locks = Vec::new();
    let deadline = tokio::time::Instant::now() + REFRESH_LOCK_WAIT;
    for path in [
        canonical_home.join(".oauth_refresh.lock"),
        PathBuf::from(legacy_path),
    ] {
        locks.push(
            crate::oauth_lock::OAuthRefreshLock::acquire(
                path,
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                Duration::from_secs(60),
                Duration::from_secs(5),
            )
            .await
            .map_err(|error| ClaudeUsageError::Refresh(error.to_string()))?,
        );
    }
    let active = read_credentials(&home).await?;
    if let Ok(token) = oauth_access_token(&active, mj_core::clock::epoch_millis())
        && rejected_token.as_deref() != Some(token)
    {
        drop(locks);
        return query_api(client, usage_url, token).await;
    }
    let before = active
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut environment = environment;
    environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        home.to_string_lossy().into_owned(),
    );
    let refresh_error = run_claude_refresh(active, environment, executor, locks)
        .await
        .err();
    let credentials = match read_credentials(&home).await {
        Ok(credentials) => credentials,
        Err(error) => return Err(refresh_error.unwrap_or(error)),
    };
    let token = match oauth_access_token(&credentials, mj_core::clock::epoch_millis()) {
        Ok(token) => token,
        Err(error) => {
            return Err(refresh_error.unwrap_or_else(|| {
                ClaudeUsageError::Refresh(format!(
                    "Claude auth login left no usable access token: {error}"
                ))
            }));
        }
    };
    if before.as_deref() == Some(token) {
        return Err(refresh_error.unwrap_or_else(|| {
            ClaudeUsageError::Refresh("Claude auth login did not renew the access token".to_owned())
        }));
    }
    query_api(client, usage_url, token).await
}

/// On macOS, read Claude's quota through its Keychain-aware CLI.
async fn run_claude_usage(
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    purpose: &'static str,
) -> Result<CommandOutput, String> {
    let mut command = CommandSpec::new(
        if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        },
        ["-p", "/usage", "--no-session-persistence"],
    )
    .purpose(purpose);
    command.env.extend(environment);
    tokio::task::spawn_blocking(move || executor.execute(&command))
        .await
        .map_err(|error| format!("worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

async fn run_claude_refresh(
    credentials: Value,
    mut environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
    locks: Vec<crate::oauth_lock::OAuthRefreshLock>,
) -> Result<(), ClaudeUsageError> {
    let oauth = &credentials["claudeAiOauth"];
    let refresh = oauth["refreshToken"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| ClaudeUsageError::Refresh("stored refresh token is missing".into()))?;
    let scopes = oauth["scopes"]
        .as_array()
        .filter(|scopes| !scopes.is_empty())
        .and_then(|scopes| scopes.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .filter(|scopes| scopes.iter().all(|scope| !scope.is_empty()))
        .ok_or_else(|| {
            ClaudeUsageError::Refresh("stored OAuth scopes are missing or invalid".into())
        })?;
    environment.insert("CLAUDE_CODE_OAUTH_REFRESH_TOKEN".into(), refresh.to_owned());
    environment.insert("CLAUDE_CODE_OAUTH_SCOPES".into(), scopes.join(" "));
    // An empty client ID selects Claude's default and overrides an ambient ID
    // belonging to another account's automation.
    environment.insert(
        "CLAUDE_CODE_OAUTH_CLIENT_ID".into(),
        oauth["clientId"].as_str().unwrap_or_default().to_owned(),
    );
    let mut command = CommandSpec::new(
        if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        },
        ["auth", "login"],
    )
    .purpose("refresh Claude login");
    command.env.extend(environment);
    command.sensitive_output = true;
    let output = tokio::task::spawn_blocking(move || {
        // Keep the leases until the executor has reaped its child, even if
        // the async caller is cancelled while this thread is still running.
        let _locks = locks;
        executor.execute(&command)
    })
    .await
    .map_err(|error| ClaudeUsageError::Refresh(format!("worker failed: {error}")))?
    .map_err(|error| ClaudeUsageError::Refresh(error.to_string()))?;
    successful_refresh_output(output)
}

fn successful_refresh_output(output: CommandOutput) -> Result<(), ClaudeUsageError> {
    if output.status == 0 {
        Ok(())
    } else {
        Err(ClaudeUsageError::Refresh(format!(
            "Claude auth login exited with status {}",
            output.status
        )))
    }
}

fn oauth_access_token(credentials: &Value, now_ms: i64) -> Result<&str, ClaudeUsageError> {
    let oauth = credentials
        .get("claudeAiOauth")
        .ok_or(ClaudeUsageError::NotSignedIn)?;
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(ClaudeUsageError::NotSignedIn)?;
    if oauth_expires_at(oauth).is_some_and(|expires_at| expires_at <= now_ms) {
        return Err(ClaudeUsageError::LoginExpired);
    }
    Ok(token)
}

fn oauth_expires_at(oauth: &Value) -> Option<i64> {
    let value = oauth.get("expiresAt")?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|ms| i64::try_from(ms).ok()))
        .or_else(|| value.as_str()?.parse().ok())
}

fn oauth_has_refresh_token(credentials: &Value) -> bool {
    credentials
        .pointer("/claudeAiOauth/refreshToken")
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty())
}

fn parse_api_usage(payload: &Value) -> Option<ClaudeUsageReport> {
    let mut five_hour = None;
    let mut week = None;
    let mut scoped_weeks = Vec::new();

    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(kind) = limit.get("kind").and_then(Value::as_str) else {
                continue;
            };
            let Some(mut window) = api_window(limit, "percent") else {
                continue;
            };
            match kind {
                "session" => {
                    window.duration_seconds = Some(5 * 60 * 60);
                    five_hour = Some(window);
                }
                "weekly_all" => {
                    window.duration_seconds = Some(7 * 24 * 60 * 60);
                    week = Some(window);
                }
                "weekly_scoped" => {
                    if let Some(scope) = api_scope_name(limit) {
                        window.duration_seconds = Some(7 * 24 * 60 * 60);
                        scoped_weeks.push((scope, window));
                    }
                }
                _ => {}
            }
        }
    } else {
        five_hour = payload
            .get("five_hour")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"));
        if let Some(mut window) = payload
            .get("seven_day")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"))
        {
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            week = Some(window);
        }
        if let Some(mut window) = payload
            .get("seven_day_fable")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"))
        {
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            scoped_weeks.push(("fable".to_owned(), window));
        }
    }

    (five_hour.is_some() || week.is_some() || !scoped_weeks.is_empty()).then(|| ClaudeUsageReport {
        banked_resets: parse_banked_resets(payload, mj_core::clock::epoch_millis() / 1000),
        five_hour,
        week,
        scoped_weeks,
    })
}

fn parse_banked_resets(payload: &Value, now: i64) -> Option<u64> {
    let block = payload
        .get("cedar_ember")
        .filter(|value| !value.is_null())?;
    let parsed = (|| {
        if !block.get("eligible")?.as_bool()? {
            return Some(0);
        }
        let mut total = 0_u64;
        for grant in block.get("grants")?.as_array()? {
            let remaining = grant.get("resets_left")?.as_u64()?;
            let timestamp = |key| -> Option<Option<i64>> {
                match grant.get(key).filter(|value| !value.is_null()) {
                    None => Some(None),
                    Some(value) => chrono::DateTime::parse_from_rfc3339(value.as_str()?)
                        .ok()
                        .map(|date| Some(date.timestamp())),
                }
            };
            if timestamp("starts_at")?.is_some_and(|start| start > now)
                || timestamp("ends_at")?.is_some_and(|end| end <= now)
            {
                continue;
            }
            // A banked reset can require reaching a limit before use.
            // `usable_now` is not the balance of unused resets.
            total = total.checked_add(remaining)?;
        }
        Some(total)
    })();
    if parsed.is_none() {
        tracing::warn!("Claude usage returned malformed reset-grant metadata");
    }
    parsed
}

fn api_window(value: &Value, percent_key: &str) -> Option<ClaudeUsageWindow> {
    let used = value.get(percent_key)?.as_f64()?;
    let used = used.round().clamp(0.0, 100.0) as u8;
    Some(ClaudeUsageWindow {
        remaining_percent: 100 - used,
        duration_seconds: None,
        reset_context: value
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Scrape `claude -p /usage` for the two windows the quota row shows.
///
/// The command prints one line per window, each naming its own percentage:
///
/// ```text
/// Current session: 6% used · resets Sep 18 at 6:20pm (Europe/Paris)
/// Current week (all models): 30% used · resets Sep 20 at 1:59pm (Europe/Paris)
/// Current week (Fable): 40% used · resets Sep 20 at 2pm (Europe/Paris)
/// ```
///
/// The sections below those quote unrelated percentages ("57% of your usage
/// was at >150k context", "Top skills: /code-review 3%"), so only a line whose
/// label is one of the windows is read, and only when the number is followed
/// by `used`. A percentage that means something else is worth reporting as
/// unparsed rather than reporting backwards.
fn parse_cli_usage(output: &str) -> Option<ClaudeUsageReport> {
    let mut five_hour = None;
    let mut week = None;
    let mut scoped_weeks = Vec::new();

    for line in strip_ansi(output).lines() {
        let Some((label, rest)) = line.split_once(':') else {
            continue;
        };
        let label = label.trim().to_ascii_lowercase();
        if label != "current session" && !label.starts_with("current week") {
            continue;
        }
        let Some(mut window) = cli_window(rest) else {
            continue;
        };
        if label == "current session" {
            window.duration_seconds = Some(5 * 60 * 60);
            five_hour = Some(window);
        } else if label == "current week (all models)" || label == "current week" {
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            week = Some(window);
        } else {
            let Some(scope) = label
                .strip_prefix("current week (")
                .and_then(|label| label.strip_suffix(')'))
            else {
                continue;
            };
            window.duration_seconds = Some(7 * 24 * 60 * 60);
            scoped_weeks.push((scope.to_owned(), window));
        }
    }

    (five_hour.is_some() || week.is_some() || !scoped_weeks.is_empty()).then_some(
        ClaudeUsageReport {
            banked_resets: None,
            five_hour,
            week,
            scoped_weeks,
        },
    )
}

/// One window from the text after a `/usage` line's label.
fn cli_window(rest: &str) -> Option<ClaudeUsageWindow> {
    let (used, tail) = rest.trim_start().split_once('%')?;
    let used: f64 = used.trim().parse().ok()?;
    if !tail.trim_start().starts_with("used") {
        return None;
    }
    Some(ClaudeUsageWindow {
        remaining_percent: 100 - (used.round().clamp(0.0, 100.0) as u8),
        duration_seconds: None,
        reset_context: tail
            .split_once("resets ")
            .map(|(_, context)| context.trim().to_owned())
            .filter(|context| !context.is_empty()),
    })
}

/// Claude Code paints `/usage` for a terminal, so its output can carry SGR
/// escapes even in print mode.
fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        // CSI sequences end at their final byte; anything else ends at the
        // next character.
        if characters.next() == Some('[') {
            for character in characters.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    output
}

fn is_authentication_error(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    [
        "not logged in",
        "not signed in",
        "unauthenticated",
        "unauthorized",
        "please log in",
        "please login",
        "invalid api key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn api_scope_name(value: &Value) -> Option<String> {
    value
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::Mutex;

    /// Captured verbatim from `claude -p /usage --no-session-persistence`.
    const CLI_USAGE: &str = "\
You are currently using your subscription to power your Claude Code usage

Current session: 6% used · resets Sep 18 at 6:20pm (Europe/Paris)
Current week (all models): 30% used · resets Sep 20 at 1:59pm (Europe/Paris)
Current week (Fable): 40% used · resets Sep 20 at 2pm (Europe/Paris)

What's contributing to your limits usage?
Approximate, based on local sessions on this machine — does not include other devices or claude.ai. Behaviors are independent characteristics, not a breakdown.

Last 24h · 2108 requests · 13 sessions
  57% of your usage was at >150k context
  41% of your usage was while 4+ sessions ran in parallel
  31% of your usage came from subagent-heavy sessions
  Top skills: /code-review 3%
  Top MCP servers: claude-in-chrome 1%

Last 7d · 5966 requests · 78 sessions
  52% of your usage was at >150k context
  28% of your usage came from subagent-heavy sessions
  28% of your usage was while 4+ sessions ran in parallel
  Top skills: /code-review 2%
  Top subagents: Explore 2%, Plan 1%
  Top MCP servers: claude-in-chrome 2%
";

    #[test]
    fn banked_resets_count_unspent_grants_without_requiring_immediate_use() {
        let payload = serde_json::json!({"cedar_ember": {"eligible": true, "grants": [
            {"resets_left": 1, "usable_now": false, "use_requires_limit": true},
            {"resets_left": 2, "starts_at": "1970-01-01T00:01:40Z", "ends_at": "1970-01-01T00:03:20Z"},
            {"resets_left": 5, "ends_at": "1970-01-01T00:01:40Z"},
            {"resets_left": 7, "starts_at": "1970-01-01T00:03:20Z"}
        ]}});
        assert_eq!(super::parse_banked_resets(&payload, 100), Some(3));
        assert_eq!(
            super::parse_banked_resets(
                &serde_json::json!({"cedar_ember": {"eligible": false}}),
                100
            ),
            Some(0)
        );
        assert_eq!(
            super::parse_banked_resets(
                &serde_json::json!({"cedar_ember": {"eligible": true, "grants": []}}),
                100
            ),
            Some(0)
        );
    }

    #[test]
    fn unknown_reset_metadata_does_not_discard_claude_usage() {
        for block in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"eligible": true}),
            serde_json::json!({"eligible": true, "grants": [{"resets_left": -1}]}),
            serde_json::json!({"eligible": true, "grants": [{"resets_left": "1"}]}),
            serde_json::json!({"eligible": true, "grants": [{"resets_left": 1, "ends_at": "invalid"}]}),
        ] {
            let report = super::parse_api_usage(&serde_json::json!({
                "five_hour": {"utilization": 25}, "cedar_ember": block
            }))
            .unwrap();
            assert_eq!(report.banked_resets, None);
            assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
        }
    }
    #[test]
    fn cli_usage_reports_both_windows_and_the_binding_weekly_limit() {
        let report = parse_cli_usage(CLI_USAGE).expect("the captured output parses");

        let five_hour = report.five_hour.expect("session window");
        assert_eq!(five_hour.remaining_percent, 94);
        assert_eq!(
            five_hour.reset_context.as_deref(),
            Some("Sep 18 at 6:20pm (Europe/Paris)")
        );
        // The all-models limit remains distinct from the Fable limit.
        let week = report.week.expect("weekly window");
        assert_eq!(week.remaining_percent, 70);
        assert_eq!(
            week.reset_context.as_deref(),
            Some("Sep 20 at 1:59pm (Europe/Paris)")
        );
        assert_eq!(report.scoped_weeks.len(), 1);
        assert_eq!(report.scoped_weeks[0].0, "fable");
        assert_eq!(report.scoped_weeks[0].1.remaining_percent, 60);
        assert_eq!(report.scoped_weeks[0].1.duration_seconds, Some(604_800));
    }

    #[test]
    fn api_usage_keeps_all_and_scoped_weekly_windows_separate() {
        let report = parse_api_usage(&serde_json::json!({
            "limits": [
                {"kind": "weekly_all", "percent": 30},
                {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}}, "percent": 40},
                {"kind": "weekly_scoped", "scope": {"model": {"display_name": "Mythos"}}, "percent": 50}
            ]
        })).unwrap();
        assert_eq!(report.week.unwrap().remaining_percent, 70);
        assert_eq!(report.scoped_weeks.len(), 2);
        assert_eq!(report.scoped_weeks[0].0, "fable");
        assert_eq!(report.scoped_weeks[0].1.remaining_percent, 60);
        assert_eq!(report.scoped_weeks[1].0, "mythos");

        let legacy = parse_api_usage(&serde_json::json!({
            "seven_day": {"utilization": 25},
            "seven_day_fable": {"utilization": 100}
        }))
        .unwrap();
        assert_eq!(legacy.week.unwrap().remaining_percent, 75);
        assert_eq!(legacy.scoped_weeks[0].0, "fable");
        assert_eq!(legacy.scoped_weeks[0].1.remaining_percent, 0);
    }

    /// The later sections are full of percentages that are not quota.
    #[test]
    fn cli_usage_ignores_percentages_that_are_not_a_window() {
        let report = parse_cli_usage(
            "Last 24h · 2108 requests · 13 sessions\n  \
             57% of your usage was at >150k context\n  \
             Top skills: /code-review 3%\n",
        );
        assert_eq!(report, None);
    }

    #[test]
    fn cli_usage_survives_terminal_colouring() {
        let coloured = "\u{1b}[1mCurrent session:\u{1b}[0m \u{1b}[32m6% used\u{1b}[0m · resets Sep 18 at 6:20pm\n";
        let report = parse_cli_usage(coloured).expect("coloured output parses");
        assert_eq!(report.five_hour.unwrap().remaining_percent, 94);
    }

    /// A window whose number stops meaning "used" must read as unparsed
    /// rather than as its own complement.
    #[test]
    fn cli_usage_refuses_a_percentage_it_cannot_interpret() {
        assert_eq!(
            parse_cli_usage("Current session: 6% left · resets Sep 18 at 6:20pm\n"),
            None
        );
    }

    #[derive(Clone)]
    struct RefreshExecutor {
        home: PathBuf,
        replacement: Option<Value>,
        status: i32,
        commands: Arc<Mutex<Vec<CommandSpec>>>,
    }

    impl CommandExecutor for RefreshExecutor {
        fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
            assert_eq!(command.args, ["auth", "login"]);
            assert_eq!(
                command.env["CLAUDE_CONFIG_DIR"],
                self.home.to_string_lossy()
            );
            assert!(self.home.join(".oauth_refresh.lock").is_dir());
            let mut legacy_path = self.home.canonicalize().unwrap().into_os_string();
            legacy_path.push(".lock");
            assert!(PathBuf::from(legacy_path).is_dir());
            self.commands.lock().unwrap().push(command.clone());
            if let Some(replacement) = &self.replacement {
                std::fs::write(
                    self.home.join(".credentials.json"),
                    serde_json::to_vec(replacement)?,
                )?;
            }
            Ok(CommandOutput {
                status: self.status,
                stdout: b"Approximate local usage".to_vec(),
                stderr: Vec::new(),
            })
        }
    }

    #[derive(Clone)]
    struct UsageServerState {
        reject_first: bool,
        authorizations: Arc<Mutex<Vec<String>>>,
    }

    async fn test_usage(
        State(state): State<UsageServerState>,
        axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(query.get("cedar_ember").map(String::as_str), Some("1"));
        assert_eq!(query.get("skip_spend").map(String::as_str), Some("1"));
        assert_eq!(
            headers.get(reqwest::header::USER_AGENT).unwrap(),
            USAGE_USER_AGENT
        );
        let authorization = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut authorizations = state.authorizations.lock().unwrap();
        let reject = state.reject_first && authorizations.is_empty();
        authorizations.push(authorization);
        drop(authorizations);
        if reject {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
        }
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "five_hour": {"utilization": 25.0, "resets_at": "2026-08-23T00:00:00Z"},
                "seven_day": {"utilization": 40.0, "resets_at": "2026-08-29T00:00:00Z"},
                "cedar_ember": {"eligible": true, "grants": [{"resets_left": 1}]}
            })),
        )
    }

    async fn spawn_usage_server(
        reject_first: bool,
    ) -> (String, UsageServerState, tokio::task::JoinHandle<()>) {
        let state = UsageServerState {
            reject_first,
            authorizations: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/usage", get(test_usage))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/usage"), state, server)
    }

    fn credentials(token: &str, refresh: Option<&str>, expires_at: i64) -> Value {
        let mut oauth = serde_json::json!({
            "accessToken": token,
            "expiresAt": expires_at,
            "scopes": ["user:profile", "user:inference"],
        });
        if let Some(refresh) = refresh {
            oauth["refreshToken"] = Value::String(refresh.to_owned());
        }
        serde_json::json!({"claudeAiOauth": oauth})
    }

    async fn usage_status_only(
        status: StatusCode,
        retry_after: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route(
            "/usage",
            get(move || async move {
                let mut headers = HeaderMap::new();
                if let Some(value) = retry_after {
                    headers.insert("retry-after", value.parse().unwrap());
                }
                (status, headers)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/usage"), server)
    }

    #[tokio::test]
    async fn a_429_is_a_rate_limit_that_carries_the_servers_retry_after() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials(
                "token",
                None,
                mj_core::clock::epoch_millis() + 3_600_000,
            ))
            .unwrap(),
        )
        .unwrap();
        for (header, expected) in [
            (Some("120"), Some(Duration::from_secs(120))),
            (None, None),
            (Some("not a delay"), None),
        ] {
            let (url, server) = usage_status_only(StatusCode::TOO_MANY_REQUESTS, header).await;
            let executor = RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: None,
                status: 0,
                commands: Arc::new(Mutex::new(Vec::new())),
            };
            let error = query_with(
                home.path().to_path_buf(),
                HashMap::new(),
                &url,
                Arc::new(executor),
            )
            .await
            .unwrap_err();
            assert_eq!(error, ClaudeUsageError::RateLimited(expected), "{header:?}");
            assert_eq!(error.to_string(), "rate limited");
            server.abort();
        }
    }

    // Hard-won: 958aba5ce: successful /usage stopped renewing expired OAuth tokens.
    #[tokio::test]
    async fn expired_login_exchanges_its_refresh_token_without_starting_a_session() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".credentials.json");
        let mut old = credentials("old", Some("refresh"), 1);
        old["claudeAiOauth"]["clientId"] = "custom-client".into();
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let fresh = credentials(
            "fresh",
            Some("rotated"),
            mj_core::clock::epoch_millis() + 60_000,
        );
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(fresh),
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(false).await;
        let mut environment = HashMap::from([(
            "CLAUDE_CONFIG_DIR".to_owned(),
            home.path().to_string_lossy().into_owned(),
        )]);

        let report = query_with(
            home.path().to_path_buf(),
            environment.clone(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        assert_eq!(report.banked_resets, Some(1));
        assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].args, ["auth", "login"]);
        environment.insert("CLAUDE_CODE_OAUTH_REFRESH_TOKEN".into(), "refresh".into());
        environment.insert(
            "CLAUDE_CODE_OAUTH_SCOPES".into(),
            "user:profile user:inference".into(),
        );
        environment.insert("CLAUDE_CODE_OAUTH_CLIENT_ID".into(), "custom-client".into());
        assert_eq!(commands[0].env, environment.into_iter().collect());
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer fresh"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn authoritative_rejection_refreshes_once_and_retries_with_new_credentials() {
        let home = tempfile::tempdir().unwrap();
        let fresh_expiry = mj_core::clock::epoch_millis() + 60_000;
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), fresh_expiry)).unwrap(),
        )
        .unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(credentials("fresh", Some("rotated"), fresh_expiry)),
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(true).await;

        query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].env["CLAUDE_CODE_OAUTH_CLIENT_ID"], "");
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer old", "Bearer fresh"]
        );
        server.abort();
    }

    struct BlockedRefreshExecutor {
        inner: RefreshExecutor,
        started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        finish: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl CommandExecutor for BlockedRefreshExecutor {
        fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
            if let Some(started) = self.started.lock().unwrap().take() {
                let _ = started.send(());
            }
            self.finish
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))?;
            self.inner.execute(command)
        }
    }

    #[tokio::test]
    async fn concurrent_quota_readers_reuse_the_token_renewed_under_claudes_lock() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let executor = Arc::new(BlockedRefreshExecutor {
            inner: RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: Some(credentials(
                    "fresh",
                    Some("rotated"),
                    mj_core::clock::epoch_millis() + 60_000,
                )),
                status: 0,
                commands: commands.clone(),
            },
            started: Mutex::new(Some(started_tx)),
            finish: Mutex::new(finish_rx),
        });
        let (usage_url, server_state, server) = spawn_usage_server(false).await;
        let first_home = home.path().to_path_buf();
        let first_url = usage_url.clone();
        let first_executor = executor.clone();
        let first = tokio::spawn(async move {
            query_with(first_home, HashMap::new(), &first_url, first_executor).await
        });
        started_rx.await.unwrap();
        let second = query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            executor,
        );
        // Poll the second reader while the first remains inside its exchange.
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut second)
                .await
                .is_err()
        );
        finish_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap();
        assert_eq!(commands.lock().unwrap().len(), 1);
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer fresh", "Bearer fresh"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_a_quota_reader_keeps_its_refresh_lock_until_the_child_finishes() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let executor = Arc::new(BlockedRefreshExecutor {
            inner: RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: None,
                status: 1,
                commands: Arc::new(Mutex::new(Vec::new())),
            },
            started: Mutex::new(Some(started_tx)),
            finish: Mutex::new(finish_rx),
        });
        let path = home.path().to_path_buf();
        let task = tokio::spawn(async move {
            query_with(path, HashMap::new(), "http://127.0.0.1:1/unused", executor).await
        });
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let lock = home.path().join(".oauth_refresh.lock");
        assert!(
            lock.is_dir(),
            "the cancelled wait still has a running child"
        );
        finish_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while lock.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_legacy_claude_lock_delays_refresh_and_the_waiter_rereads_the_renewed_token() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".credentials.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let mut legacy = home.path().canonicalize().unwrap().into_os_string();
        legacy.push(".lock");
        let legacy = PathBuf::from(legacy);
        std::fs::create_dir(&legacy).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: None,
            status: 0,
            commands: commands.clone(),
        });
        let (url, state, server) = spawn_usage_server(false).await;
        let query = query_with(home.path().to_path_buf(), HashMap::new(), &url, executor);
        tokio::pin!(query);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut query)
                .await
                .is_err()
        );
        assert!(commands.lock().unwrap().is_empty());
        assert!(home.path().join(".oauth_refresh.lock").is_dir());
        std::fs::write(
            &path,
            serde_json::to_vec(&credentials(
                "fresh",
                Some("rotated"),
                mj_core::clock::epoch_millis() + 60_000,
            ))
            .unwrap(),
        )
        .unwrap();
        std::fs::remove_dir(legacy).unwrap();
        query.await.unwrap();
        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(
            state.authorizations.lock().unwrap().as_slice(),
            ["Bearer fresh"]
        );
        server.abort();
    }

    // Hard-won: 958aba5ce: a successful CLI exit did not prove that credentials were renewed.
    #[tokio::test]
    async fn successful_cli_without_token_renewal_is_reported_as_a_refresh_failure() {
        for expires_at in [1, mj_core::clock::epoch_millis() + 60_000] {
            let home = tempfile::tempdir().unwrap();
            std::fs::write(
                home.path().join(".credentials.json"),
                serde_json::to_vec(&credentials("old", Some("refresh"), expires_at)).unwrap(),
            )
            .unwrap();
            let executor = Arc::new(RefreshExecutor {
                home: home.path().to_path_buf(),
                replacement: None,
                status: 0,
                commands: Arc::new(Mutex::new(Vec::new())),
            });
            let (url, _, server) = spawn_usage_server(true).await;
            let error = query_with(home.path().to_path_buf(), HashMap::new(), &url, executor)
                .await
                .unwrap_err();
            assert!(matches!(error, ClaudeUsageError::Refresh(_)), "{error}");
            assert!(!home.path().join(".oauth_refresh.lock").exists());
            server.abort();
        }
    }

    #[tokio::test]
    async fn valid_credentials_query_authoritative_usage_without_launching_claude() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials(
                "current",
                Some("refresh"),
                mj_core::clock::epoch_millis() + 60_000,
            ))
            .unwrap(),
        )
        .unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: None,
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(false).await;

        query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer current"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn failed_refresh_of_a_rejected_token_is_not_mislabeled_login_expired() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials(
                "rejected",
                Some("refresh"),
                mj_core::clock::epoch_millis() + 60_000,
            ))
            .unwrap(),
        )
        .unwrap();
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: None,
            status: 1,
            commands: Arc::new(Mutex::new(Vec::new())),
        };
        let (usage_url, server_state, server) = spawn_usage_server(true).await;

        let error = query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ClaudeUsageError::Refresh(_)));
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer rejected"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn failed_cli_is_accepted_when_credentials_were_refreshed_concurrently() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(credentials(
                "fresh",
                Some("rotated"),
                mj_core::clock::epoch_millis() + 60_000,
            )),
            status: 1,
            commands: Arc::new(Mutex::new(Vec::new())),
        };
        let (usage_url, _, server) = spawn_usage_server(false).await;

        let report = query_with(
            home.path().to_path_buf(),
            HashMap::new(),
            &usage_url,
            Arc::new(executor),
        )
        .await
        .unwrap();

        assert_eq!(report.week.unwrap().remaining_percent, 60);
        server.abort();
    }

    #[test]
    fn expired_oauth_access_token_is_login_expired() {
        let credentials = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-test",
                "expiresAt": 1_000
            }
        });
        assert_eq!(
            oauth_access_token(&credentials, 1_001),
            Err(ClaudeUsageError::LoginExpired)
        );
        assert_eq!(
            oauth_access_token(&credentials, 1_000),
            Err(ClaudeUsageError::LoginExpired)
        );
    }

    #[test]
    fn current_oauth_access_token_is_usable() {
        let credentials = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat01-test",
                "expiresAt": 2_000
            }
        });
        assert_eq!(
            oauth_access_token(&credentials, 1_999).expect("token"),
            "sk-ant-oat01-test"
        );
    }
}
