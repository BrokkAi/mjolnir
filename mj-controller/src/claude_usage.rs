//! Claude Code `/usage` polling and parsing.
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

use hel::hel_targets::{CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec};

const USAGE_TIMEOUT: Duration = Duration::from_secs(20);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Quota-row copy when the stored Claude OAuth access token is past `expiresAt`.
pub(crate) const LOGIN_EXPIRED: &str = "login expired";

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(test)]
pub enum ClaudeUsageStatus {
    Available(ClaudeUsageReport),
    Unavailable(String),
}

#[cfg(test)]
impl ClaudeUsageStatus {
    pub fn compact_label(&self) -> String {
        match self {
            Self::Available(report) => report.compact_label(),
            Self::Unavailable(reason) => format!("Claude usage unavailable: {reason}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageReport {
    pub five_hour: Option<ClaudeUsageWindow>,
    pub week: Option<ClaudeUsageWindow>,
}

#[cfg(test)]
impl ClaudeUsageReport {
    pub fn compact_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(window) = &self.five_hour {
            parts.push(window.compact_label("5H"));
        }
        if let Some(window) = &self.week {
            parts.push(window.compact_label("Week"));
        }

        if parts.is_empty() {
            "Claude usage: unavailable".to_string()
        } else {
            format!("Claude usage: {}", parts.join(" · "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageWindow {
    pub remaining_percent: u8,
    /// Text following `reset` in Claude Code output, without the word itself.
    pub reset_context: Option<String>,
}

#[cfg(test)]
impl ClaudeUsageWindow {
    fn compact_label(&self, label: &str) -> String {
        let mut text = format!("{label} {}% left", self.remaining_percent);
        if let Some(reset_context) = &self.reset_context {
            text.push_str(" · resets ");
            text.push_str(reset_context);
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeUsageError {
    TimedOut,
    #[cfg(test)]
    NotInstalled,
    NotSignedIn,
    LoginExpired,
    Refresh(String),
    #[cfg(test)]
    Launch(String),
    Query(String),
    #[cfg(test)]
    Exit {
        status: String,
        detail: String,
    },
    #[cfg(test)]
    UnsupportedOutput,
    Parse,
}

#[cfg(test)]
impl ClaudeUsageError {
    pub fn user_reason(&self) -> &'static str {
        match self {
            Self::TimedOut => "request timed out",
            Self::NotInstalled => "Claude Code not installed",
            Self::NotSignedIn => "not signed in",
            Self::LoginExpired => LOGIN_EXPIRED,
            Self::Refresh(_) => "could not refresh Claude login",
            Self::Launch(_) => "could not launch Claude Code",
            Self::Query(_) => "could not query Claude usage",
            Self::Exit { detail, .. } if is_authentication_error(detail) => "not signed in",
            Self::Exit { .. } => "Claude /usage failed",
            Self::UnsupportedOutput => "Claude /usage is unsupported",
            Self::Parse => "unrecognized Claude /usage response",
        }
    }
}

impl fmt::Display for ClaudeUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => write!(f, "claude /usage timed out"),
            #[cfg(test)]
            Self::NotInstalled => write!(f, "Claude Code executable not found"),
            Self::NotSignedIn => write!(f, "Claude Code is not signed in"),
            Self::LoginExpired => write!(f, "{LOGIN_EXPIRED}"),
            Self::Refresh(error) => write!(f, "refresh Claude login: {error}"),
            #[cfg(test)]
            Self::Launch(error) => write!(f, "run claude /usage: {error}"),
            Self::Query(error) => write!(f, "query Claude usage: {error}"),
            #[cfg(test)]
            Self::Exit { status, detail } if detail.is_empty() => {
                write!(f, "claude /usage exited with {status}")
            }
            #[cfg(test)]
            Self::Exit { status, detail } => {
                write!(f, "claude /usage exited with {status}: {detail}")
            }
            #[cfg(test)]
            Self::UnsupportedOutput => write!(f, "Claude Code does not support /usage"),
            Self::Parse => write!(f, "could not parse claude /usage output"),
        }
    }
}

/// Query the same OAuth usage endpoint as Claude Code's interactive `/usage`.
/// The CLI is invoked only to refresh rejected credentials; its print-mode
/// usage output is approximate, so the API response remains authoritative.
pub async fn query(
    home: PathBuf,
    environment: HashMap<String, String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    query_with(
        home,
        environment,
        USAGE_URL,
        Arc::new(CancellableProcessExecutor::with_timeout(REFRESH_TIMEOUT)),
    )
    .await
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
    match oauth_access_token(&credentials, hel::clock::epoch_millis()) {
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
    let refresh_error = run_claude_refresh(environment, executor).await.err();
    let credentials = match read_credentials(&home).await {
        Ok(credentials) => credentials,
        Err(error) => return Err(refresh_error.unwrap_or(error)),
    };
    let token = match oauth_access_token(&credentials, hel::clock::epoch_millis()) {
        Ok(token) => token,
        Err(error) => return Err(refresh_error.unwrap_or(error)),
    };
    if rejected_token.as_deref() == Some(token)
        && let Some(error) = refresh_error
    {
        return Err(error);
    }
    query_api(client, usage_url, token).await
}

async fn run_claude_refresh(
    environment: HashMap<String, String>,
    executor: Arc<dyn CommandExecutor + Send + Sync>,
) -> Result<(), ClaudeUsageError> {
    let mut command = CommandSpec::new(
        if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        },
        ["-p", "/usage", "--no-session-persistence"],
    )
    .purpose("refresh Claude login");
    command.env.extend(environment);
    let output = tokio::task::spawn_blocking(move || executor.execute(&command))
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
            "Claude /usage exited with status {}",
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
    let mut weekly = Vec::new();

    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(kind) = limit.get("kind").and_then(Value::as_str) else {
                continue;
            };
            let Some(window) = api_window(limit, "percent") else {
                continue;
            };
            match kind {
                "session" => five_hour = Some(window),
                "weekly_all" => weekly.push(window),
                "weekly_scoped" if api_scope_name(limit).is_some_and(|name| name == "fable") => {
                    weekly.push(window);
                }
                _ => {}
            }
        }
    } else {
        five_hour = payload
            .get("five_hour")
            .filter(|value| !value.is_null())
            .and_then(|value| api_window(value, "utilization"));
        for key in ["seven_day", "seven_day_fable"] {
            if let Some(window) = payload
                .get(key)
                .filter(|value| !value.is_null())
                .and_then(|value| api_window(value, "utilization"))
            {
                weekly.push(window);
            }
        }
    }

    let week = weekly
        .into_iter()
        .min_by_key(|window| window.remaining_percent);
    (five_hour.is_some() || week.is_some()).then_some(ClaudeUsageReport { five_hour, week })
}

fn api_window(value: &Value, percent_key: &str) -> Option<ClaudeUsageWindow> {
    let used = value.get(percent_key)?.as_f64()?;
    let used = used.round().clamp(0.0, 100.0) as u8;
    Some(ClaudeUsageWindow {
        remaining_percent: 100 - used,
        reset_context: value
            .get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn api_scope_name(value: &Value) -> Option<String> {
    value
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
}

/// Scrape Claude Code `/usage` output for the two quota windows we display.
///
/// The command output has changed shape across Claude Code releases (plain
/// lines, markdown-ish tables, and the ACP metadata wording all show up in the
/// wild), so the parser intentionally keys off semantic labels plus nearby
/// percentage words rather than a single exact template.
#[cfg(test)]
pub fn parse(output: &str) -> Option<ClaudeUsageReport> {
    let stripped = strip_ansi(output);
    let lines = stripped
        .lines()
        .map(normalize_line)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    let report = ClaudeUsageReport {
        five_hour: parse_window(&lines, UsageWindowKind::FiveHour),
        week: parse_window(&lines, UsageWindowKind::Week),
    };

    (report.five_hour.is_some() || report.week.is_some()).then_some(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum UsageWindowKind {
    FiveHour,
    Week,
}

#[cfg(test)]
fn parse_window(lines: &[String], kind: UsageWindowKind) -> Option<ClaudeUsageWindow> {
    let mut fallback = None;
    let mut preferred = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if !matches_window(line, kind) {
            continue;
        }

        let section = section_around(lines, idx, kind);
        let parsed = parse_window_section(&section).map(|mut window| {
            window.reset_context = reset_context(lines, idx, kind);
            window
        });
        if let Some(window) = parsed {
            if preferred_window_line(line, kind) {
                if kind == UsageWindowKind::FiveHour {
                    return Some(window);
                }
                preferred.push(window);
            } else {
                fallback.get_or_insert(window);
            }
        }
    }

    preferred
        .into_iter()
        .min_by_key(|window| window.remaining_percent)
        .or(fallback)
}

#[cfg(test)]
fn reset_context(lines: &[String], start: usize, kind: UsageWindowKind) -> Option<String> {
    lines
        .iter()
        .skip(start)
        .take(5)
        .take_while(|line| !matches_any_window(line) || matches_window(line, kind))
        .find_map(|line| reset_context_in_line(line))
}

#[cfg(test)]
fn reset_context_in_line(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let reset_start = lower.find("reset")?;
    let after_reset = line[reset_start + "reset".len()..].trim_start();
    let after_plural = after_reset.strip_prefix('s').unwrap_or(after_reset);
    let after_reset = if after_plural.starts_with(char::is_whitespace) {
        after_plural
    } else {
        after_reset
    };
    let context = after_reset
        .trim_start_matches(|ch: char| ch == ':' || ch == '-' || ch.is_whitespace())
        .trim();
    (!context.is_empty()).then(|| context.chars().take(96).collect())
}

#[cfg(test)]
fn section_around(lines: &[String], start: usize, kind: UsageWindowKind) -> String {
    let mut section = String::new();
    if let Some(header) = lines[..start]
        .iter()
        .rev()
        .take(3)
        .find(|line| quota_percent_header(line))
    {
        section.push_str(header);
        section.push(' ');
    }
    section.push_str(&lines[start]);
    // Some Claude Code builds render a label on one line and the percentages on
    // following rows. Carry a small local window, stopping when a different
    // quota heading starts.
    for line in lines.iter().skip(start + 1).take(4) {
        if matches_any_window(line) && !matches_window(line, kind) {
            break;
        }
        section.push(' ');
        section.push_str(line);
    }
    section
}

#[cfg(test)]
fn quota_percent_header(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("used")
        && (lower.contains("remaining") || lower.contains("left") || lower.contains("available"))
}

#[cfg(test)]
fn preferred_window_line(line: &str, kind: UsageWindowKind) -> bool {
    if kind != UsageWindowKind::Week {
        return true;
    }
    let lower = line.to_ascii_lowercase();
    // Claude may report both the overall allowance and a separate Fable
    // allowance. Both constrain weekly usage, while model-specific buckets do
    // not represent the subscription-wide limit shown by Hel.
    lower.contains("fable")
        || lower.contains("all models")
        || lower.contains("overall")
        || (!lower.contains('(') && !lower.contains("opus") && !lower.contains("sonnet"))
}

#[cfg(test)]
fn matches_any_window(line: &str) -> bool {
    matches_window(line, UsageWindowKind::FiveHour) || matches_window(line, UsageWindowKind::Week)
}

#[cfg(test)]
fn matches_window(line: &str, kind: UsageWindowKind) -> bool {
    let lower = line.to_ascii_lowercase();
    match kind {
        UsageWindowKind::FiveHour => {
            lower.contains("5-hour")
                || lower.contains("5 hour")
                || lower.contains("5h")
                || lower.contains("five-hour")
                || lower.contains("five hour")
                || lower.contains("current session")
        }
        UsageWindowKind::Week => {
            lower.contains("weekly")
                || lower.contains("current week")
                || lower.contains("7-day")
                || lower.contains("7 day")
                || lower.contains("seven-day")
                || lower.contains("seven day")
                || lower.contains("week")
        }
    }
}

#[cfg(test)]
fn parse_window_section(section: &str) -> Option<ClaudeUsageWindow> {
    let percents = percentages(section);
    if percents.is_empty() {
        return None;
    }

    let lower = section.to_ascii_lowercase();

    if percents.len() >= 2
        && lower.contains("used")
        && (lower.contains("remaining") || lower.contains("left") || lower.contains("available"))
    {
        // Claude's table shape is `Used | Remaining`, so the remaining
        // percentage is the later cell. This also handles prose like
        // `used 12% · remaining 88%`.
        return percents.last().map(|percent| ClaudeUsageWindow {
            remaining_percent: percent.value,
            reset_context: None,
        });
    }

    if let Some(value) = percents
        .iter()
        .find(|percent| context_for(&lower, percent).contains("remaining"))
        .or_else(|| {
            percents
                .iter()
                .find(|percent| context_for(&lower, percent).contains("left"))
        })
        .or_else(|| {
            percents
                .iter()
                .find(|percent| context_for(&lower, percent).contains("available"))
        })
        .map(|percent| percent.value)
    {
        return Some(ClaudeUsageWindow {
            remaining_percent: value,
            reset_context: None,
        });
    }

    if let Some(used) = percents.iter().find_map(|percent| {
        let context = context_for(&lower, percent);
        (context.contains("used") || context.contains("usage") || context.contains("utilization"))
            .then_some(percent.value)
    }) {
        return Some(ClaudeUsageWindow {
            remaining_percent: 100u8.saturating_sub(used),
            reset_context: None,
        });
    }

    // Markdown tables often have headers (`used`, `remaining`) far enough from
    // the cells that the local context above cannot see them. When both words
    // exist in the section, Claude's table places the remaining percentage
    // after the used percentage.
    if lower.contains("remaining") || lower.contains("left") || lower.contains("available") {
        return percents.last().map(|percent| ClaudeUsageWindow {
            remaining_percent: percent.value,
            reset_context: None,
        });
    }

    if lower.contains("used") || lower.contains("usage") || lower.contains("utilization") {
        return percents.first().map(|percent| ClaudeUsageWindow {
            remaining_percent: 100u8.saturating_sub(percent.value),
            reset_context: None,
        });
    }

    // Last-resort fallback: a labeled quota line with a single percentage is
    // more likely to be a remaining quota than unrelated text, and showing a
    // stale/missing row is worse than showing the scraped value.
    (percents.len() == 1).then(|| ClaudeUsageWindow {
        remaining_percent: percents[0].value,
        reset_context: None,
    })
}

#[cfg(test)]
fn classify_unparsed_output(output: &str) -> ClaudeUsageError {
    let lower = output.to_ascii_lowercase();
    if is_authentication_error(&lower) {
        ClaudeUsageError::NotSignedIn
    } else if lower.contains("not supported") || lower.contains("unknown command") {
        ClaudeUsageError::UnsupportedOutput
    } else {
        ClaudeUsageError::Parse
    }
}

#[cfg(test)]
fn is_authentication_error(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    [
        "not logged in",
        "not signed in",
        "unauthenticated",
        "unauthorized",
        "authentication",
        "please log in",
        "please login",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[derive(Debug, Clone, Copy)]
#[cfg(test)]
struct Percent {
    value: u8,
    start: usize,
    end: usize,
}

#[cfg(test)]
fn percentages(text: &str) -> Vec<Percent> {
    let mut out = Vec::new();
    let mut iter = text.char_indices().peekable();

    while let Some((start, ch)) = iter.next() {
        if !ch.is_ascii_digit() {
            continue;
        }

        let mut number = String::from(ch);
        while let Some(&(_, next)) = iter.peek() {
            if next.is_ascii_digit() || next == '.' {
                number.push(next);
                iter.next();
            } else {
                break;
            }
        }

        while let Some(&(_, next)) = iter.peek() {
            if next.is_whitespace() {
                iter.next();
            } else {
                break;
            }
        }

        let Some(&(percent_idx, '%')) = iter.peek() else {
            continue;
        };
        iter.next();
        let end = percent_idx + 1;

        if let Ok(value) = number.parse::<f64>() {
            out.push(Percent {
                value: value.round().clamp(0.0, 100.0) as u8,
                start,
                end,
            });
        }
    }

    out
}

#[cfg(test)]
fn context_for<'a>(lower: &'a str, percent: &Percent) -> &'a str {
    let start = lower_floor_char_boundary(lower, percent.start.saturating_sub(40));
    let end = lower_ceil_char_boundary(lower, (percent.end + 40).min(lower.len()));
    &lower[start..end]
}

#[cfg(test)]
fn lower_floor_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
fn lower_ceil_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
fn normalize_line(line: &str) -> String {
    line.chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|ch: char| ch == '│' || ch == '|' || ch == '─' || ch.is_whitespace())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct RefreshExecutor {
        home: PathBuf,
        replacement: Option<Value>,
        status: i32,
        commands: Arc<Mutex<Vec<CommandSpec>>>,
    }

    impl CommandExecutor for RefreshExecutor {
        fn execute(&self, command: &CommandSpec) -> anyhow::Result<CommandOutput> {
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
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
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
                "seven_day": {"utilization": 40.0, "resets_at": "2026-08-29T00:00:00Z"}
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
        });
        if let Some(refresh) = refresh {
            oauth["refreshToken"] = Value::String(refresh.to_owned());
        }
        serde_json::json!({"claudeAiOauth": oauth})
    }

    #[tokio::test]
    async fn expired_login_asks_claude_to_refresh_without_persisting_a_session() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".credentials.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&credentials("old", Some("refresh"), 1)).unwrap(),
        )
        .unwrap();
        let fresh = credentials(
            "fresh",
            Some("rotated"),
            hel::clock::epoch_millis() + 60_000,
        );
        let commands = Arc::new(Mutex::new(Vec::new()));
        let executor = RefreshExecutor {
            home: home.path().to_path_buf(),
            replacement: Some(fresh),
            status: 0,
            commands: commands.clone(),
        };
        let (usage_url, server_state, server) = spawn_usage_server(false).await;
        let environment = HashMap::from([(
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

        assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].args,
            ["-p", "/usage", "--no-session-persistence"]
        );
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
        let fresh_expiry = hel::clock::epoch_millis() + 60_000;
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

        assert_eq!(commands.lock().unwrap().len(), 1);
        assert_eq!(
            server_state.authorizations.lock().unwrap().as_slice(),
            ["Bearer old", "Bearer fresh"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn valid_credentials_query_authoritative_usage_without_launching_claude() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".credentials.json"),
            serde_json::to_vec(&credentials(
                "current",
                Some("refresh"),
                hel::clock::epoch_millis() + 60_000,
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
                hel::clock::epoch_millis() + 60_000,
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
                hel::clock::epoch_millis() + 60_000,
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
    fn parses_remaining_percent_lines() {
        let report = parse(
            r#"
            Claude Code Usage
            5-hour limit: 88% remaining · resets at 4:30pm
            Weekly limit: 63% remaining · resets Monday
            "#,
        )
        .expect("report");

        assert_eq!(report.five_hour.as_ref().unwrap().remaining_percent, 88);
        assert_eq!(report.week.as_ref().unwrap().remaining_percent, 63);
        assert_eq!(
            report.five_hour.as_ref().unwrap().reset_context.as_deref(),
            Some("at 4:30pm")
        );
        assert_eq!(
            report.week.as_ref().unwrap().reset_context.as_deref(),
            Some("Monday")
        );
        assert_eq!(
            report.compact_label(),
            "Claude usage: 5H 88% left · resets at 4:30pm · Week 63% left · resets Monday"
        );
    }

    #[test]
    fn parses_used_percent_lines_from_acp_wording() {
        let report = parse(
            r#"
            Current session: 8% used · resets Jun 17 at 4:49pm
            Current week (all models): 34% used · resets Jun 21 at 9:00am
            "#,
        )
        .expect("report");

        assert_eq!(report.five_hour.unwrap().remaining_percent, 92);
        assert_eq!(report.week.unwrap().remaining_percent, 66);
    }

    #[test]
    fn parses_actual_claude_usage_output_shape() {
        let report = parse(
            r#"
            You are currently using your subscription to power your Claude Code usage

            Current session: 2% used · resets Jul 1, 12:40pm (Europe/Paris)
            Current week (all models): 27% used · resets Jul 2, 1am (Europe/Paris)

            What's contributing to your limits usage?
            Approximate, based on local sessions on this machine — does not include other devices or claude.ai. Behaviors are independent characteristics, not a breakdown.

            Last 24h · 2265 requests · 29 sessions
              52% of your usage came from subagent-heavy sessions
              51% of your usage was at >150k context
              Top skills: /review 3%
              Top subagents: workflow-subagent 12%, review 3%

            Last 7d · 7808 requests · 67 sessions
              85% of your usage came from subagent-heavy sessions
              68% of your usage was at >150k context
              Top skills: /brokk:review-pr 3%, /review 1%
              Top subagents: brokk:review-pr 4%, workflow-subagent 3%, Explore 1%, general-purpose 1%, review 1%
              Top plugins: brokk 7%
              Top MCP servers: brokk 2%, ccd_session 2%
            "#,
        )
        .expect("report");

        assert_eq!(report.five_hour.as_ref().unwrap().remaining_percent, 98);
        assert_eq!(report.week.as_ref().unwrap().remaining_percent, 73);
        assert_eq!(
            report.compact_label(),
            "Claude usage: 5H 98% left · resets Jul 1, 12:40pm (Europe/Paris) · Week 73% left · resets Jul 2, 1am (Europe/Paris)"
        );
    }

    #[test]
    fn parses_markdown_table_shape() {
        let report = parse(
            r#"
            | Window | Used | Remaining |
            | 5-hour | 12% | 88% |
            | Weekly | 37% | 63% |
            "#,
        )
        .expect("report");

        assert_eq!(report.five_hour.unwrap().remaining_percent, 88);
        assert_eq!(report.week.unwrap().remaining_percent, 63);
    }

    #[test]
    fn parses_reset_from_following_table_row() {
        let report = parse(
            r#"
            Current session
            12% used
            Resets 4:30pm (America/Chicago)
            Current week (all models)
            37% used
            Resets Aug 14 at 9am (America/Chicago)
            "#,
        )
        .expect("report");

        assert_eq!(
            report.five_hour.unwrap().reset_context.as_deref(),
            Some("4:30pm (America/Chicago)")
        );
        assert_eq!(
            report.week.unwrap().reset_context.as_deref(),
            Some("Aug 14 at 9am (America/Chicago)")
        );
    }

    #[test]
    fn prefers_global_week_over_model_specific_week() {
        let report = parse(
            r#"
            Current week (Opus): 90% used
            Current week (all models): 34% used
            "#,
        )
        .expect("report");

        assert_eq!(report.week.unwrap().remaining_percent, 66);
    }

    #[test]
    fn weekly_usage_uses_lower_of_overall_and_fable_remaining() {
        let report = parse(
            r#"
            Current week (all models): 20% used · resets Aug 20 at 9am
            Current week (Fable): 65% used · resets Aug 21 at 10am
            "#,
        )
        .expect("report");

        let week = report.week.unwrap();
        assert_eq!(week.remaining_percent, 35);
        assert_eq!(week.reset_context.as_deref(), Some("Aug 21 at 10am"));
    }

    #[test]
    fn weekly_usage_uses_overall_when_it_has_less_remaining_than_fable() {
        let report = parse(
            r#"
            Current week (Fable): 10% used · resets Aug 21 at 10am
            Current week (all models): 70% used · resets Aug 20 at 9am
            "#,
        )
        .expect("report");

        assert_eq!(report.week.unwrap().remaining_percent, 30);
    }

    #[test]
    fn api_usage_uses_exhausted_fable_limit_over_overall_limit() {
        let report = parse_api_usage(&serde_json::json!({
            "limits": [
                {
                    "kind": "session",
                    "percent": 13.0,
                    "resets_at": "2026-08-18T23:30:00Z"
                },
                {
                    "kind": "weekly_all",
                    "percent": 96.0,
                    "resets_at": "2026-08-19T22:59:00Z"
                },
                {
                    "kind": "weekly_scoped",
                    "percent": 100.0,
                    "resets_at": "2026-08-19T22:59:00Z",
                    "scope": { "model": { "display_name": "Fable" } }
                }
            ]
        }))
        .expect("report");

        assert_eq!(report.five_hour.unwrap().remaining_percent, 87);
        let week = report.week.unwrap();
        assert_eq!(week.remaining_percent, 0);
        assert_eq!(week.reset_context.as_deref(), Some("2026-08-19T22:59:00Z"));
    }

    #[test]
    fn api_usage_ignores_other_model_scoped_weekly_limits() {
        let report = parse_api_usage(&serde_json::json!({
            "limits": [
                { "kind": "weekly_all", "percent": 40.0 },
                {
                    "kind": "weekly_scoped",
                    "percent": 90.0,
                    "scope": { "model": { "display_name": "Opus" } }
                },
                {
                    "kind": "weekly_scoped",
                    "percent": 50.0,
                    "scope": { "model": { "display_name": "Fable" } }
                }
            ]
        }))
        .expect("report");

        assert_eq!(report.week.unwrap().remaining_percent, 50);
    }

    #[test]
    fn strips_ansi_sequences() {
        let report = parse("\u{1b}[32m5H quota: 75% left\u{1b}[0m").expect("report");

        assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
    }

    #[test]
    fn status_labels_and_error_reasons_are_concise() {
        let available = ClaudeUsageStatus::Available(ClaudeUsageReport {
            five_hour: Some(ClaudeUsageWindow {
                remaining_percent: 75,
                reset_context: Some("at 4:30pm".to_string()),
            }),
            week: None,
        });
        assert_eq!(
            available.compact_label(),
            "Claude usage: 5H 75% left · resets at 4:30pm"
        );
        assert_eq!(
            ClaudeUsageStatus::Unavailable("not signed in".to_string()).compact_label(),
            "Claude usage unavailable: not signed in"
        );
        assert_eq!(
            ClaudeUsageError::TimedOut.user_reason(),
            "request timed out"
        );
        assert_eq!(
            ClaudeUsageError::NotInstalled.user_reason(),
            "Claude Code not installed"
        );
        assert_eq!(ClaudeUsageError::NotSignedIn.user_reason(), "not signed in");
        assert_eq!(ClaudeUsageError::LoginExpired.user_reason(), LOGIN_EXPIRED);
        assert_eq!(
            ClaudeUsageError::Launch("permission denied".to_string()).user_reason(),
            "could not launch Claude Code"
        );
        assert_eq!(
            ClaudeUsageError::Exit {
                status: "exit status: 1".to_string(),
                detail: "authentication required".to_string(),
            }
            .user_reason(),
            "not signed in"
        );
        assert_eq!(
            ClaudeUsageError::Exit {
                status: "exit status: 1".to_string(),
                detail: "temporary failure".to_string(),
            }
            .user_reason(),
            "Claude /usage failed"
        );
        assert_eq!(
            ClaudeUsageError::UnsupportedOutput.user_reason(),
            "Claude /usage is unsupported"
        );
        assert_eq!(
            ClaudeUsageError::Parse.user_reason(),
            "unrecognized Claude /usage response"
        );
        assert_eq!(
            classify_unparsed_output("unknown command: /usage"),
            ClaudeUsageError::UnsupportedOutput
        );
        assert_eq!(
            classify_unparsed_output("Please log in to continue"),
            ClaudeUsageError::NotSignedIn
        );
        assert_eq!(classify_unparsed_output("hello"), ClaudeUsageError::Parse);
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

    #[test]
    fn oauth_access_token_without_expiry_is_usable() {
        let credentials = serde_json::json!({
            "claudeAiOauth": { "accessToken": "sk-ant-oat01-test" }
        });
        assert_eq!(
            oauth_access_token(&credentials, 9_000).expect("token"),
            "sk-ant-oat01-test"
        );
    }

    #[test]
    fn missing_oauth_access_token_is_not_signed_in() {
        assert_eq!(
            oauth_access_token(&serde_json::json!({}), 1),
            Err(ClaudeUsageError::NotSignedIn)
        );
        assert_eq!(
            oauth_access_token(
                &serde_json::json!({ "claudeAiOauth": { "accessToken": "" } }),
                1
            ),
            Err(ClaudeUsageError::NotSignedIn)
        );
    }
}
