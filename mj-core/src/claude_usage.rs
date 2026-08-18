//! Claude Code `/usage` polling and parsing.
//!
//! The Claude ACP agent exposes token usage over ACP, but the subscription
//! quota shown by Claude Code lives behind its local `/usage` command.  Keep
//! this module independent from the UI state machine so the parser can be
//! tested against captured command output without spawning `claude`.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::usage_fact::{StoredFact, UsageFactStore};

const USAGE_TIMEOUT: Duration = Duration::from_secs(20);
const RUNTIME_PREPARE_TIMEOUT: Duration = Duration::from_secs(180);
static CLAUDE_RUNTIME_READY: OnceCell<()> = OnceCell::const_new();

/// Provider key of the machine-wide shared `/usage` fact.
const SHARED_FACT_PROVIDER: &str = "claude";
/// How long a published fact satisfies [`query`] before someone probes
/// again. Matches the quota gate's in-process cache TTL.
const SHARED_FACT_TTL: Duration = Duration::from_secs(60);
/// Checkout lease on the shared fact: long enough to cover first-run
/// runtime preparation plus the probe itself, short enough that a
/// crashed holder does not block other processes for long.
const CHECKOUT_LEASE: Duration = RUNTIME_PREPARE_TIMEOUT
    .saturating_add(USAGE_TIMEOUT)
    .saturating_add(Duration::from_secs(10));
/// How often waiters re-read the shared fact while another process
/// holds the checkout lease.
const CHECKOUT_POLL: Duration = Duration::from_millis(500);

pub use crate::provider_usage::{ClaudeUsageReport, ClaudeUsageStatus, ClaudeUsageWindow};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeUsageError {
    TimedOut,
    NotInstalled,
    NotSignedIn,
    Launch(String),
    Exit { status: String, detail: String },
    UnsupportedOutput,
    Parse,
}

impl ClaudeUsageError {
    pub fn user_reason(&self) -> &'static str {
        match self {
            Self::TimedOut => "request timed out",
            Self::NotInstalled => "Claude Code not installed",
            Self::NotSignedIn => "not signed in",
            Self::Launch(_) => "could not launch Claude Code",
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
            Self::NotInstalled => write!(f, "Claude Code executable not found"),
            Self::NotSignedIn => write!(f, "Claude Code is not signed in"),
            Self::Launch(error) => write!(f, "run claude /usage: {error}"),
            Self::Exit { status, detail } if detail.is_empty() => {
                write!(f, "claude /usage exited with {status}")
            }
            Self::Exit { status, detail } => {
                write!(f, "claude /usage exited with {status}: {detail}")
            }
            Self::UnsupportedOutput => write!(f, "Claude Code does not support /usage"),
            Self::Parse => write!(f, "could not parse claude /usage output"),
        }
    }
}

/// Claude subscription usage via the machine-wide shared fact.
///
/// Every mj process (TUI instances across worktrees, `mj server`,
/// headless runs) shares one `/usage` fact in a small sqlite database.
/// A fresh fact is returned directly; a stale one is refreshed by
/// whichever caller wins the checkout lease while everyone else waits
/// for the published result — so N concurrent mj instances spawn one
/// `claude -p /usage` probe, not N.
pub async fn query(
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    query_shared(cwd, env, SHARED_FACT_TTL).await
}

/// Like [`query`] but ignores the shared fact's age (still serialized
/// through the checkout lease). Used to recheck quota right after an
/// agent failure, where a minute-old "clear" answer is not good enough.
pub async fn query_fresh(
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    query_shared(cwd, env, Duration::ZERO).await
}

async fn query_shared(
    cwd: PathBuf,
    env: HashMap<String, String>,
    max_age: Duration,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let store = UsageFactStore::new(crate::usage_fact::default_store_path());
    query_shared_with(store, max_age, move || probe(cwd, env)).await
}

async fn query_shared_with<F, Fut>(
    store: UsageFactStore,
    max_age: Duration,
    probe: F,
) -> Result<ClaudeUsageReport, ClaudeUsageError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<ClaudeUsageReport, ClaudeUsageError>>,
{
    // A per-process sequence keeps concurrent queries from sharing an
    // owner id: `try_checkout` treats a matching owner as a renewal, so
    // colliding owners would both win the lease and both probe.
    static OWNER_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let started = crate::usage_fact::unix_now();
    let owner = format!(
        "mj-{}-{}",
        std::process::id(),
        OWNER_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let deadline = Instant::now() + CHECKOUT_LEASE + USAGE_TIMEOUT;
    loop {
        if let Some(result) = read_fact(&store)
            .await
            .filter(|fact| fact_is_current(fact, started, max_age))
            .and_then(|fact| decode_fact(&fact))
        {
            return result;
        }
        if checkout_fact(&store, &owner).await {
            let result = probe().await;
            match serde_json::to_string(&result) {
                Ok(payload) => publish_fact(&store, payload, owner).await,
                Err(_) => release_fact(&store, owner).await,
            }
            return result;
        }
        if Instant::now() >= deadline {
            // Waited out an entire lease without a publish. Fall back to
            // any existing fact rather than blocking even longer.
            if let Some(result) = read_fact(&store).await.and_then(|fact| decode_fact(&fact)) {
                return result;
            }
            return Err(ClaudeUsageError::TimedOut);
        }
        tokio::time::sleep(CHECKOUT_POLL).await;
    }
}

/// A fact is current when it satisfies the caller's age bound as of the
/// call start, or was published after the call began (a concurrent
/// lease holder just finished — that result is as fresh as our own
/// probe would have been).
fn fact_is_current(fact: &StoredFact, started: i64, max_age: Duration) -> bool {
    if max_age.is_zero() {
        // Timestamps have second granularity, so a fact stamped in the
        // same second as the call may predate it. A forced recheck must
        // never accept a possibly pre-failure answer; re-probing in that
        // rare tie is the cheaper mistake.
        return fact.fetched_at > started;
    }
    let max_age = i64::try_from(max_age.as_secs()).unwrap_or(i64::MAX);
    fact.fetched_at >= started || started.saturating_sub(fact.fetched_at) <= max_age
}

/// `None` when the payload does not deserialize — e.g. written by a
/// different mj version. The caller then probes and overwrites it.
fn decode_fact(fact: &StoredFact) -> Option<Result<ClaudeUsageReport, ClaudeUsageError>> {
    serde_json::from_str(&fact.payload).ok()
}

async fn read_fact(store: &UsageFactStore) -> Option<StoredFact> {
    let store = store.clone();
    match tokio::task::spawn_blocking(move || store.read(SHARED_FACT_PROVIDER)).await {
        Ok(Ok(fact)) => fact,
        Ok(Err(error)) => {
            tracing::debug!("read shared Claude usage fact: {error}");
            None
        }
        Err(_) => None,
    }
}

/// A storage failure counts as a successful checkout: the shared cache
/// must never make usage reporting worse than probing directly.
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
            tracing::debug!("checkout shared Claude usage fact: {error}");
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
        tracing::debug!("publish shared Claude usage fact: {error}");
    }
}

async fn release_fact(store: &UsageFactStore, owner: String) {
    let store = store.clone();
    let result =
        tokio::task::spawn_blocking(move || store.release(SHARED_FACT_PROVIDER, &owner)).await;
    if let Ok(Err(error)) = result {
        tracing::debug!("release shared Claude usage fact: {error}");
    }
}

/// Run the Claude executable bundled with `claude-agent-acp` and parse its
/// `/usage` summary.
async fn probe(
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<ClaudeUsageReport, ClaudeUsageError> {
    let prepared = crate::acp::prepare_provider_cli(crate::acp::ProviderCli::Claude, &env)
        .await
        .map_err(|error| ClaudeUsageError::Launch(error.to_string()))?;
    ensure_runtime_ready(&prepared, &cwd).await?;
    let output = run_cli(&prepared, &cwd, &["-p", "/usage"], USAGE_TIMEOUT).await?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}\n{stderr}");
        if is_authentication_error(&combined) {
            return Err(ClaudeUsageError::NotSignedIn);
        }
        let detail = combined
            .split_whitespace()
            .take(24)
            .collect::<Vec<_>>()
            .join(" ");
        return Err(ClaudeUsageError::Exit {
            status: output.status.to_string(),
            detail,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.trim().is_empty() {
        stdout.into_owned()
    } else if stdout.trim().is_empty() {
        stderr.into_owned()
    } else {
        format!("{stdout}\n{stderr}")
    };

    parse(&combined).ok_or_else(|| classify_unparsed_output(&combined))
}

async fn ensure_runtime_ready(
    prepared: &crate::acp::PreparedProviderCli,
    cwd: &std::path::Path,
) -> Result<(), ClaudeUsageError> {
    CLAUDE_RUNTIME_READY
        .get_or_try_init(|| async {
            let output = run_cli(prepared, cwd, &["--version"], RUNTIME_PREPARE_TIMEOUT)
                .await
                .map_err(|error| match error {
                    ClaudeUsageError::TimedOut => ClaudeUsageError::Launch(
                        "bundled Claude runtime preparation timed out".to_string(),
                    ),
                    other => other,
                })?;
            if output.status.success() {
                Ok(())
            } else {
                let detail = output_detail(&output);
                Err(ClaudeUsageError::Launch(format!(
                    "bundled Claude runtime preparation failed: {detail}"
                )))
            }
        })
        .await
        .copied()
}

async fn run_cli(
    prepared: &crate::acp::PreparedProviderCli,
    cwd: &std::path::Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, ClaudeUsageError> {
    let mut command = Command::new(&prepared.command);
    command
        .args(&prepared.args)
        .args(args)
        .current_dir(cwd)
        .envs(&prepared.env)
        .stderr(Stdio::piped());
    crate::acp::configure_isolated_child(&mut command, crate::acp::SpawnIsolation::ProcessGroup);
    // Quota polling is non-interactive. Override the helper's piped stdin
    // after applying its process-group contract.
    command.stdin(Stdio::null());

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ClaudeUsageError::NotInstalled
        } else {
            ClaudeUsageError::Launch(error.to_string())
        }
    })?;
    let pid = child.id();
    let mut stdout = child.stdout.take().ok_or_else(|| {
        ClaudeUsageError::Launch("bundled Claude runtime stdout was not captured".to_string())
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        ClaudeUsageError::Launch("bundled Claude runtime stderr was not captured".to_string())
    })?;
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|error| ClaudeUsageError::Launch(error.to_string()))?,
        Err(_) => {
            if let Err(error) = crate::acp::kill_agent_tree(&mut child, pid).await {
                tracing::warn!("reap timed-out Claude usage process tree: {error:#}");
            }
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(ClaudeUsageError::TimedOut);
        }
    };
    let teardown = crate::acp::kill_agent_tree(&mut child, pid).await;
    let stdout = stdout_task
        .await
        .map_err(|error| ClaudeUsageError::Launch(error.to_string()))?
        .map_err(|error| ClaudeUsageError::Launch(error.to_string()))?;
    let stderr = stderr_task
        .await
        .map_err(|error| ClaudeUsageError::Launch(error.to_string()))?
        .map_err(|error| ClaudeUsageError::Launch(error.to_string()))?;
    if let Err(error) = teardown {
        return Err(ClaudeUsageError::Launch(format!(
            "reap Claude usage process tree: {error:#}"
        )));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn output_detail(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = format!("{stdout}\n{stderr}")
        .split_whitespace()
        .take(24)
        .collect::<Vec<_>>()
        .join(" ");
    if detail.is_empty() {
        output.status.to_string()
    } else {
        detail
    }
}

/// Scrape Claude Code `/usage` output for the two quota windows we display.
///
/// The command output has changed shape across Claude Code releases (plain
/// lines, markdown-ish tables, and the ACP metadata wording all show up in the
/// wild), so the parser intentionally keys off semantic labels plus nearby
/// percentage words rather than a single exact template.
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
enum UsageWindowKind {
    FiveHour,
    Week,
}

fn parse_window(lines: &[String], kind: UsageWindowKind) -> Option<ClaudeUsageWindow> {
    let mut fallback = None;

    for (idx, line) in lines.iter().enumerate() {
        if !matches_window(line, kind) {
            continue;
        }

        let section = section_around(lines, idx, kind);
        let parsed = parse_window_section(&section).map(|mut window| {
            window.reset_context = reset_context(lines, idx, kind);
            window
        });
        if parsed.is_some() && preferred_window_line(line, kind) {
            return parsed;
        }
        fallback = fallback.or(parsed);
    }

    fallback
}

fn reset_context(lines: &[String], start: usize, kind: UsageWindowKind) -> Option<String> {
    lines
        .iter()
        .skip(start)
        .take(5)
        .take_while(|line| !matches_any_window(line) || matches_window(line, kind))
        .find_map(|line| reset_context_in_line(line))
}

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

fn quota_percent_header(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("used")
        && (lower.contains("remaining") || lower.contains("left") || lower.contains("available"))
}

fn preferred_window_line(line: &str, kind: UsageWindowKind) -> bool {
    if kind != UsageWindowKind::Week {
        return true;
    }
    let lower = line.to_ascii_lowercase();
    // Prefer the global weekly bucket when Claude also emits model-specific
    // weekly buckets such as Opus/Sonnet.
    !lower.contains("opus") && !lower.contains("sonnet")
}

fn matches_any_window(line: &str) -> bool {
    matches_window(line, UsageWindowKind::FiveHour) || matches_window(line, UsageWindowKind::Week)
}

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
struct Percent {
    value: u8,
    start: usize,
    end: usize,
}

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

fn context_for<'a>(lower: &'a str, percent: &Percent) -> &'a str {
    let start = lower_floor_char_boundary(lower, percent.start.saturating_sub(40));
    let end = lower_ceil_char_boundary(lower, (percent.end + 40).min(lower.len()));
    &lower[start..end]
}

fn lower_floor_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn lower_ceil_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

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

    fn shared_store() -> (tempfile::TempDir, UsageFactStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = UsageFactStore::new(dir.path().join("usage.sqlite3"));
        (dir, store)
    }

    fn sample_report() -> ClaudeUsageReport {
        ClaudeUsageReport {
            five_hour: Some(ClaudeUsageWindow {
                remaining_percent: 88,
                reset_context: Some("at 4:30pm".to_string()),
            }),
            week: Some(ClaudeUsageWindow {
                remaining_percent: 63,
                reset_context: None,
            }),
        }
    }

    async fn failing_probe() -> Result<ClaudeUsageReport, ClaudeUsageError> {
        panic!("the shared fact should have satisfied this query")
    }

    #[tokio::test]
    async fn fresh_shared_fact_short_circuits_the_probe() {
        let (_dir, store) = shared_store();
        let payload = serde_json::to_string(&Ok::<_, ClaudeUsageError>(sample_report()))
            .expect("serialize fact");
        store
            .publish(
                SHARED_FACT_PROVIDER,
                &payload,
                "seed",
                crate::usage_fact::unix_now(),
            )
            .expect("publish");

        let result = query_shared_with(store, SHARED_FACT_TTL, failing_probe).await;
        assert_eq!(result, Ok(sample_report()));
    }

    #[tokio::test]
    async fn stale_store_probes_once_and_publishes_for_later_queries() {
        let (_dir, store) = shared_store();
        let result = query_shared_with(store.clone(), SHARED_FACT_TTL, || async {
            Ok(sample_report())
        })
        .await;
        assert_eq!(result, Ok(sample_report()));

        // The probe result became the shared fact, so a second query is
        // answered from the store.
        let result = query_shared_with(store, SHARED_FACT_TTL, failing_probe).await;
        assert_eq!(result, Ok(sample_report()));
    }

    #[tokio::test]
    async fn probe_errors_are_shared_to_avoid_stampedes() {
        let (_dir, store) = shared_store();
        let result = query_shared_with(store.clone(), SHARED_FACT_TTL, || async {
            Err(ClaudeUsageError::NotSignedIn)
        })
        .await;
        assert_eq!(result, Err(ClaudeUsageError::NotSignedIn));

        let result = query_shared_with(store, SHARED_FACT_TTL, failing_probe).await;
        assert_eq!(result, Err(ClaudeUsageError::NotSignedIn));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn waiter_returns_the_fact_published_by_the_lease_holder() {
        let (_dir, store) = shared_store();
        store
            .try_checkout(
                SHARED_FACT_PROVIDER,
                "other-process",
                CHECKOUT_LEASE,
                crate::usage_fact::unix_now(),
            )
            .expect("checkout");

        let publisher = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let payload = serde_json::to_string(&Ok::<_, ClaudeUsageError>(sample_report()))
                .expect("serialize fact");
            publisher
                .publish(
                    SHARED_FACT_PROVIDER,
                    &payload,
                    "other-process",
                    crate::usage_fact::unix_now(),
                )
                .expect("publish");
        });

        let result = query_shared_with(store, SHARED_FACT_TTL, failing_probe).await;
        assert_eq!(result, Ok(sample_report()));
    }

    #[test]
    fn stale_facts_are_current_again_once_republished() {
        let fact = StoredFact {
            payload: String::new(),
            fetched_at: 1000,
        };
        assert!(fact_is_current(&fact, 1030, Duration::from_secs(60)));
        assert!(!fact_is_current(&fact, 1090, Duration::from_secs(60)));
        // A forced refresh only accepts facts strictly newer than the
        // call: a same-second fact could predate the failure that
        // triggered the recheck.
        assert!(fact_is_current(&fact, 999, Duration::ZERO));
        assert!(!fact_is_current(&fact, 1000, Duration::ZERO));
        // Same-second is fine for ordinary TTL-bounded queries.
        assert!(fact_is_current(&fact, 1000, Duration::from_secs(60)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_usage_query_reaps_the_wrapper_process_group() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("claude-wrapper");
        let child_pid = temp.path().join("child.pid");
        std::fs::write(
            &script,
            "#!/bin/sh\nsleep 30 &\necho \"$!\" > \"$CLAUDE_USAGE_CHILD_PID\"\nwait\n",
        )
        .expect("write wrapper");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make wrapper executable");
        let prepared = crate::acp::PreparedProviderCli {
            command: script,
            args: Vec::new(),
            env: HashMap::from([(
                "CLAUDE_USAGE_CHILD_PID".to_string(),
                child_pid.to_string_lossy().into_owned(),
            )]),
        };

        assert!(matches!(
            run_cli(&prepared, temp.path(), &[], Duration::from_millis(100)).await,
            Err(ClaudeUsageError::TimedOut)
        ));
        let pid = std::fs::read_to_string(child_pid)
            .expect("child pid")
            .trim()
            .parse::<libc::pid_t>()
            .expect("numeric child pid");
        let mut gone = false;
        for _ in 0..20 {
            // SAFETY: signal 0 only checks whether the recorded process exists.
            let result = unsafe { libc::kill(pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "timed-out Claude usage child {pid} survived cleanup");
    }

    /// #737: a CLI that exits cleanly is already reaped when the teardown
    /// tree-kill runs. On Windows, taskkill's "process not found" complaint
    /// must not replace the successful output with a launch error.
    #[tokio::test]
    async fn clean_cli_exit_preserves_output_despite_reaped_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        #[cfg(windows)]
        let prepared = crate::acp::PreparedProviderCli {
            command: "cmd".into(),
            args: ["/C", "echo", "usage-ok"].map(String::from).to_vec(),
            env: HashMap::new(),
        };
        #[cfg(unix)]
        let prepared = crate::acp::PreparedProviderCli {
            command: "sh".into(),
            args: ["-c", "echo usage-ok"].map(String::from).to_vec(),
            env: HashMap::new(),
        };

        let output = run_cli(&prepared, temp.path(), &[], Duration::from_secs(30))
            .await
            .expect("clean CLI exit must not surface a teardown failure");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("usage-ok"),
            "stdout must survive teardown: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
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
            "Claude usage: 5H 88% left · resets at 4:30pm · week 63% left · resets Monday"
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

            Current session: 2% used · resets Jul 1 at 12:40pm (Europe/Paris)
            Current week (all models): 27% used · resets Jul 2 at 1am (Europe/Paris)

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
            "Claude usage: 5H 98% left · resets Jul 1 at 12:40pm (Europe/Paris) · week 73% left · resets Jul 2 at 1am (Europe/Paris)"
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
}
