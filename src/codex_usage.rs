//! Codex subscription quota scraping.
//!
//! Codex records rate-limit snapshots in its session rollout logs at
//! `$CODEX_HOME/sessions/<year>/<month>/<day>/rollout-*.jsonl`. The Codex ACP
//! bridge forwards only context-window token usage over ACP, not these
//! subscription limits, so read the most recent rollout that carries a
//! `rate_limits` snapshot and surface the remaining quota. Keep the parser
//! independent from the UI state machine so it can be tested against captured
//! rollout lines without touching the real Codex home.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

/// Cap the newest rollout files scanned when the freshest one has no
/// `rate_limits` snapshot yet (e.g. a session that only just started).
const MAX_ROLLOUTS_SCANNED: usize = 8;

/// How many recent day directories to look at. Codex keeps appending to a
/// session's original day file even after midnight, so a single day directory
/// is not enough to guarantee we see the actively-updated rollout.
const MAX_DAY_DIRS: usize = 3;

/// The outcome of a Codex quota scrape, ready for display. Modeling the
/// unavailable case explicitly lets the UI show a short reason instead of
/// silently dropping the row when Codex has recorded nothing usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexUsageStatus {
    Available(CodexUsageReport),
    Unavailable(String),
}

impl CodexUsageStatus {
    pub fn compact_label(&self, now_unix: i64) -> String {
        match self {
            CodexUsageStatus::Available(report) => report.compact_label(now_unix),
            CodexUsageStatus::Unavailable(reason) => {
                format!("Codex usage: unavailable — {reason}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageReport {
    /// The rolling ~5-hour limit (Codex `primary`).
    pub five_hour: Option<CodexUsageWindow>,
    /// The weekly limit (Codex `secondary`).
    pub weekly: Option<CodexUsageWindow>,
    /// Subscription plan (e.g. `pro`), when Codex reports one.
    pub plan_type: Option<String>,
}

impl CodexUsageReport {
    pub fn compact_label(&self, now_unix: i64) -> String {
        let mut parts = Vec::new();
        if let Some(window) = &self.five_hour {
            parts.push(format!("5H {}", window.summary(now_unix)));
        }
        if let Some(window) = &self.weekly {
            parts.push(format!("week {}", window.summary(now_unix)));
        }

        let head = match self.plan_type.as_deref() {
            Some(plan) if !plan.is_empty() => format!("Codex usage ({plan})"),
            _ => "Codex usage".to_string(),
        };

        if parts.is_empty() {
            format!("{head}: unavailable")
        } else {
            format!("{head}: {}", parts.join(" · "))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexUsageWindow {
    pub remaining_percent: u8,
    /// Unix seconds when this window resets, when Codex reports it.
    pub resets_at: Option<i64>,
}

impl CodexUsageWindow {
    fn summary(&self, now_unix: i64) -> String {
        match self.resets_at.and_then(|at| reset_hint(at, now_unix)) {
            Some(hint) => format!("{}% left ({hint})", self.remaining_percent),
            None => format!("{}% left", self.remaining_percent),
        }
    }
}

/// Resolve the Codex home directory, honoring `CODEX_HOME` from the agent's
/// environment first (the process actually writing the rollouts), then this
/// process's environment, then `~/.codex`.
pub fn codex_home(env: &HashMap<String, String>) -> PathBuf {
    if let Some(dir) = env.get("CODEX_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .map(|home| home.join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

/// Read the newest Codex rollout that carries a rate-limit snapshot and return
/// the remaining subscription quota. Blocking filesystem work runs on the
/// blocking pool so the async runtime is never stalled.
pub async fn query(codex_home: PathBuf) -> Result<CodexUsageReport, String> {
    tokio::task::spawn_blocking(move || query_blocking(&codex_home))
        .await
        .map_err(|e| format!("codex usage task failed: {e}"))?
}

fn query_blocking(codex_home: &Path) -> Result<CodexUsageReport, String> {
    let sessions_dir = codex_home.join("sessions");
    if !sessions_dir.is_dir() {
        return Err("no Codex session logs found".to_string());
    }

    let files = recent_rollout_files(&sessions_dir, MAX_ROLLOUTS_SCANNED);
    if files.is_empty() {
        return Err("no Codex session logs found".to_string());
    }

    for file in files {
        if let Ok(contents) = fs::read_to_string(&file)
            && let Some(report) = parse_rollout(&contents)
        {
            return Ok(report);
        }
    }

    Err("no Codex quota data recorded yet".to_string())
}

/// Collect the newest rollout files across the most recent day directories,
/// ranked by real modification time (newest first).
fn recent_rollout_files(sessions_dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut collected: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut day_dirs_with_files = 0usize;

    'scan: for year in sorted_subdirs_desc(sessions_dir) {
        for month in sorted_subdirs_desc(&year) {
            for day in sorted_subdirs_desc(&month) {
                let files = rollout_files_in(&day);
                if files.is_empty() {
                    continue;
                }
                for file in files {
                    let mtime = file_mtime(&file);
                    collected.push((mtime, file));
                }
                day_dirs_with_files += 1;
                if day_dirs_with_files >= MAX_DAY_DIRS {
                    break 'scan;
                }
            }
        }
    }

    // Newest modification time first.
    collected.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    collected.truncate(limit);
    collected.into_iter().map(|(_, path)| path).collect()
}

fn sorted_subdirs_desc(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    };
    // Codex zero-pads the date components, so lexical order matches
    // chronological order and we can descend newest-first.
    dirs.sort();
    dirs.reverse();
    dirs
}

fn rollout_files_in(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_rollout_file(path))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn is_rollout_file(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

fn file_mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Scan rollout lines for the last `rate_limits` snapshot and convert it into a
/// remaining-quota report. Codex emits one `token_count` event per turn, so the
/// final snapshot in the file is the freshest.
pub fn parse_rollout(contents: &str) -> Option<CodexUsageReport> {
    let mut latest: Option<RateLimits> = None;

    for line in contents.lines() {
        let line = line.trim();
        // Cheap prefilter: skip the many lines (messages, tool calls, reasoning)
        // that cannot carry rate limits before paying for JSON parsing.
        if line.is_empty() || !line.contains("rate_limits") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<RolloutLine>(line) else {
            continue;
        };
        let rate_limits = parsed
            .payload
            .and_then(|payload| payload.rate_limits)
            .or(parsed.rate_limits);
        if let Some(limits) = rate_limits
            && (limits.primary.is_some() || limits.secondary.is_some())
        {
            latest = Some(limits);
        }
    }

    latest.and_then(report_from_rate_limits)
}

fn report_from_rate_limits(limits: RateLimits) -> Option<CodexUsageReport> {
    let five_hour = limits.primary.and_then(window_from);
    let weekly = limits.secondary.and_then(window_from);
    let plan_type = limits.plan_type.filter(|plan| !plan.is_empty());

    (five_hour.is_some() || weekly.is_some()).then_some(CodexUsageReport {
        five_hour,
        weekly,
        plan_type,
    })
}

fn window_from(window: RateWindow) -> Option<CodexUsageWindow> {
    let used = window.used_percent?;
    let remaining = (100.0 - used).round().clamp(0.0, 100.0) as u8;
    Some(CodexUsageWindow {
        remaining_percent: remaining,
        resets_at: window.resets_at,
    })
}

/// Format how long until a window resets. `None` once it has already reset so
/// the caller drops a stale/negative hint rather than showing "resets in 0".
fn reset_hint(resets_at: i64, now_unix: i64) -> Option<String> {
    let delta = resets_at - now_unix;
    if delta <= 0 {
        return None;
    }
    let hint = if delta < 3_600 {
        format!("resets in {}m", (delta / 60).max(1))
    } else if delta < 86_400 {
        format!("resets in {}h", delta / 3_600)
    } else {
        format!("resets in {}d", delta / 86_400)
    };
    Some(hint)
}

#[derive(Deserialize)]
struct RolloutLine {
    #[serde(default)]
    payload: Option<Payload>,
    // Some rollout shapes place `rate_limits` at the top level rather than under
    // `payload`; accept either.
    #[serde(default)]
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(default)]
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct RateLimits {
    #[serde(default)]
    primary: Option<RateWindow>,
    #[serde(default)]
    secondary: Option<RateWindow>,
    #[serde(default)]
    plan_type: Option<String>,
}

#[derive(Deserialize)]
struct RateWindow {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    resets_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const PRIMARY_RESETS_AT: i64 = 1_782_930_701;
    const SECONDARY_RESETS_AT: i64 = 1_783_401_094;
    // A fixed "now" a little before the primary window resets: primary in a few
    // hours, secondary in several days.
    const NOW: i64 = 1_782_920_000;

    fn token_count_line(primary_used: f64, secondary_used: f64) -> String {
        format!(
            r#"{{"timestamp":"2026-07-01T13:36:32.557Z","type":"event_msg","payload":{{"type":"token_count","info":{{"model_context_window":258400}},"rate_limits":{{"limit_id":"codex","limit_name":null,"primary":{{"used_percent":{primary_used},"window_minutes":300,"resets_at":{PRIMARY_RESETS_AT}}},"secondary":{{"used_percent":{secondary_used},"window_minutes":10080,"resets_at":{SECONDARY_RESETS_AT}}},"credits":null,"individual_limit":null,"plan_type":"pro","rate_limit_reached_type":null}}}}}}"#
        )
    }

    #[test]
    fn parses_latest_rate_limits_from_rollout() {
        let rollout = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"session_meta","payload":{"id":"abc"}}"#,
            token_count_line(3.0, 15.0),
            token_count_line(42.0, 18.0),
        );

        let report = parse_rollout(&rollout).expect("report");
        assert_eq!(report.five_hour.as_ref().unwrap().remaining_percent, 58);
        assert_eq!(report.weekly.as_ref().unwrap().remaining_percent, 82);
        assert_eq!(report.plan_type.as_deref(), Some("pro"));
        assert_eq!(
            report.five_hour.as_ref().unwrap().resets_at,
            Some(PRIMARY_RESETS_AT)
        );
    }

    #[test]
    fn ignores_null_and_unrelated_lines() {
        let rollout = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hi"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":null}}"#,
            token_count_line(10.0, 20.0),
        );

        let report = parse_rollout(&rollout).expect("report");
        assert_eq!(report.five_hour.unwrap().remaining_percent, 90);
        assert_eq!(report.weekly.unwrap().remaining_percent, 80);
    }

    #[test]
    fn returns_none_without_any_rate_limits() {
        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"abc"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"hi"}}"#,
            "\n",
        );
        assert!(parse_rollout(rollout).is_none());
    }

    #[test]
    fn rounds_and_clamps_remaining_percent() {
        // used_percent above 100 must not underflow the u8 remaining value.
        let rollout = token_count_line(150.5, 0.4);
        let report = parse_rollout(&rollout).expect("report");
        assert_eq!(report.five_hour.unwrap().remaining_percent, 0);
        assert_eq!(report.weekly.unwrap().remaining_percent, 100);
    }

    #[test]
    fn compact_label_includes_windows_plan_and_reset_hints() {
        let report = parse_rollout(&token_count_line(3.0, 15.0)).expect("report");
        assert_eq!(
            report.compact_label(NOW),
            "Codex usage (pro): 5H 97% left (resets in 2h) · week 85% left (resets in 5d)"
        );
    }

    #[test]
    fn compact_label_drops_reset_hint_once_window_has_passed() {
        let window = CodexUsageWindow {
            remaining_percent: 40,
            resets_at: Some(NOW - 10),
        };
        assert_eq!(window.summary(NOW), "40% left");
    }

    #[test]
    fn status_label_reports_unavailable_reason() {
        let status = CodexUsageStatus::Unavailable("no Codex session logs found".to_string());
        assert_eq!(
            status.compact_label(NOW),
            "Codex usage: unavailable — no Codex session logs found"
        );
    }

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mjolnir-codex-usage-{}-{tag}-{n}",
            std::process::id(),
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn query_blocking_reads_newest_rollout() {
        let home = unique_temp_dir("ok");
        let day = home.join("sessions").join("2026").join("07").join("01");
        fs::create_dir_all(&day).expect("create day dir");
        fs::write(
            day.join("rollout-2026-07-01T15-31-14-aaaa.jsonl"),
            format!("{}\n", token_count_line(25.0, 30.0)),
        )
        .expect("write rollout");

        let report = query_blocking(&home).expect("report");
        assert_eq!(report.five_hour.unwrap().remaining_percent, 75);
        assert_eq!(report.weekly.unwrap().remaining_percent, 70);

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn query_blocking_reports_unavailable_without_sessions() {
        let home = unique_temp_dir("empty");

        let err = query_blocking(&home).expect_err("should be unavailable");
        assert_eq!(err, "no Codex session logs found");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn query_blocking_reports_unavailable_without_rate_limits() {
        let home = unique_temp_dir("norates");
        let day = home.join("sessions").join("2026").join("07").join("01");
        fs::create_dir_all(&day).expect("create day dir");
        fs::write(
            day.join("rollout-2026-07-01T15-31-14-bbbb.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"abc\"}}\n",
        )
        .expect("write rollout");

        let err = query_blocking(&home).expect_err("should be unavailable");
        assert_eq!(err, "no Codex quota data recorded yet");

        fs::remove_dir_all(&home).ok();
    }
}
