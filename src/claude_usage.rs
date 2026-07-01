//! Claude Code `/usage` polling and parsing.
//!
//! The Claude ACP agent exposes token usage over ACP, but the subscription
//! quota shown by Claude Code lives behind its local `/usage` command.  Keep
//! this module independent from the UI state machine so the parser can be
//! tested against captured command output without spawning `claude`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;

const USAGE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeUsageReport {
    pub five_hour: Option<ClaudeUsageWindow>,
    pub week: Option<ClaudeUsageWindow>,
}

impl ClaudeUsageReport {
    pub fn compact_label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(window) = &self.five_hour {
            parts.push(format!("5H {}% left", window.remaining_percent));
        }
        if let Some(window) = &self.week {
            parts.push(format!("week {}% left", window.remaining_percent));
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
}

/// Run `claude -p "/usage"` and parse the resulting quota summary.
pub async fn query(
    cwd: PathBuf,
    env: HashMap<String, String>,
) -> Result<ClaudeUsageReport, String> {
    let mut cmd = Command::new(claude_program());
    cmd.arg("-p")
        .arg("/usage")
        .current_dir(cwd)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(USAGE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "claude /usage timed out".to_string())?
        .map_err(|e| format!("run claude /usage: {e}"))?;

    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr)
            .split_whitespace()
            .take(24)
            .collect::<Vec<_>>()
            .join(" ");
        return Err(if detail.is_empty() {
            format!("claude /usage exited with {}", output.status)
        } else {
            format!("claude /usage exited with {}: {detail}", output.status)
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

    parse(&combined).ok_or_else(|| "could not parse claude /usage output".to_string())
}

fn claude_program() -> &'static str {
    if cfg!(windows) {
        "claude.cmd"
    } else {
        "claude"
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
        let parsed = parse_window_section(&section);
        if parsed.is_some() && preferred_window_line(line, kind) {
            return parsed;
        }
        fallback = fallback.or(parsed);
    }

    fallback
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
        });
    }

    if let Some(used) = percents.iter().find_map(|percent| {
        let context = context_for(&lower, percent);
        (context.contains("used") || context.contains("usage") || context.contains("utilization"))
            .then_some(percent.value)
    }) {
        return Some(ClaudeUsageWindow {
            remaining_percent: 100u8.saturating_sub(used),
        });
    }

    // Markdown tables often have headers (`used`, `remaining`) far enough from
    // the cells that the local context above cannot see them. When both words
    // exist in the section, Claude's table places the remaining percentage
    // after the used percentage.
    if lower.contains("remaining") || lower.contains("left") || lower.contains("available") {
        return percents.last().map(|percent| ClaudeUsageWindow {
            remaining_percent: percent.value,
        });
    }

    if lower.contains("used") || lower.contains("usage") || lower.contains("utilization") {
        return percents.first().map(|percent| ClaudeUsageWindow {
            remaining_percent: 100u8.saturating_sub(percent.value),
        });
    }

    // Last-resort fallback: a labeled quota line with a single percentage is
    // more likely to be a remaining quota than unrelated text, and showing a
    // stale/missing row is worse than showing the scraped value.
    (percents.len() == 1).then(|| ClaudeUsageWindow {
        remaining_percent: percents[0].value,
    })
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
            report.compact_label(),
            "Claude usage: 5H 88% left · week 63% left"
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
            "Claude usage: 5H 98% left · week 73% left"
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
}
