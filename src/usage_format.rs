//! Shared formatting for live-session, provider quota, and rate-limit displays.

use chrono::{DateTime, Datelike, Days, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone};

/// Format a Unix reset timestamp as wall-clock time in the machine's local
/// time zone. Accepts seconds or milliseconds and rejects non-finite or
/// out-of-range values.
pub(crate) fn format_reset_local(epoch: f64) -> Option<String> {
    if !epoch.is_finite() {
        return None;
    }
    let seconds = if epoch.abs() >= 1_000_000_000_000.0 {
        (epoch / 1000.0).trunc() as i64
    } else {
        epoch.trunc() as i64
    };
    let local = Local.timestamp_opt(seconds, 0).single()?;
    Some(format_reset_label(local.fixed_offset()))
}

pub(crate) fn format_reset_local_seconds(epoch: i64) -> Option<String> {
    format_reset_local(epoch as f64)
}

/// Normalize a provider's textual reset value to the compact 24-hour form
/// used by the dashboard. A time-only value is the next occurrence of that
/// wall-clock time; Claude Code uses this shape for its five-hour window.
pub(crate) fn normalize_reset_text(value: &str) -> Option<String> {
    normalize_reset_at(value, Local::now().fixed_offset()).map(format_reset_label)
}

pub(crate) fn normalize_reset_epoch_seconds(value: &str) -> Option<i64> {
    normalize_reset_at(value, Local::now().fixed_offset()).map(|reset| reset.timestamp())
}

fn normalize_reset_at(value: &str, now: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(epoch) = value.parse::<f64>() {
        let seconds = if epoch.abs() >= 1_000_000_000_000.0 {
            (epoch / 1000.0).trunc() as i64
        } else {
            epoch.trunc() as i64
        };
        return Local
            .timestamp_opt(seconds, 0)
            .single()
            .map(|reset| reset.fixed_offset());
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.with_timezone(&Local).fixed_offset());
    }

    let value = value
        .strip_prefix("at ")
        .unwrap_or(value)
        .split('(')
        .next()
        .unwrap_or(value)
        .trim()
        .trim_end_matches(',');
    let parse_time = |value: &str| {
        let value = value
            .to_ascii_lowercase()
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        let value = ["am", "pm"]
            .into_iter()
            .find_map(|suffix| {
                let hour = value.strip_suffix(suffix)?;
                (!hour.contains(':')).then(|| format!("{hour}:00{suffix}"))
            })
            .unwrap_or(value);
        ["%I:%M%P", "%I%P", "%H:%M"]
            .iter()
            .find_map(|format| NaiveTime::parse_from_str(&value, format).ok())
    };

    // Claude has used both `Aug 14 at 4am` and `Aug 14, 4am` across
    // releases. Keep the provider punctuation out of the date/time parsers.
    let dated_time = value.split_once(" at ").or_else(|| {
        value
            .split_once(',')
            .map(|(date, time)| (date, time.trim()))
    });
    if let Some((date, time)) = dated_time {
        let time = parse_time(time.trim())?;
        let date = date.trim().trim_end_matches(',');
        let date = match date.to_ascii_lowercase().as_str() {
            "today" => now.date_naive(),
            "tomorrow" => now.date_naive().checked_add_days(Days::new(1))?,
            _ => NaiveDate::parse_from_str(
                &format!("{} {}", date.replace(',', ""), now.year()),
                "%b %e %Y",
            )
            .ok()?,
        };
        return now
            .timezone()
            .from_local_datetime(&date.and_time(time))
            .single();
    }

    let time = parse_time(value)?;
    let mut date = now.date_naive();
    let mut reset = now
        .timezone()
        .from_local_datetime(&date.and_time(time))
        .single()?;
    if reset <= now {
        date = date.checked_add_days(Days::new(1))?;
        reset = now
            .timezone()
            .from_local_datetime(&date.and_time(time))
            .single()?;
    }
    Some(reset)
}

/// Render a running clock: the two largest units that still fit, so a glance
/// reads the magnitude rather than counting colons.
///
/// `36s`, `43m36s`, `1h43m`, `2d03h`. Every live clock in Mjolnir uses this,
/// which is why it lives here rather than beside any one of them.
pub fn format_clock(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    match seconds {
        seconds if seconds < MINUTE => format!("{seconds}s"),
        seconds if seconds < HOUR => format!("{}m{:02}s", seconds / MINUTE, seconds % MINUTE),
        seconds if seconds < DAY => format!("{}h{:02}m", seconds / HOUR, (seconds % HOUR) / MINUTE),
        seconds => format!("{}d{:02}h", seconds / DAY, (seconds % DAY) / HOUR),
    }
}

/// Render a session's current-turn clock. A session with no turn in flight
/// reads `[idle]` rather than showing an empty cell.
pub fn format_turn_clock(now_epoch_seconds: u64, current_turn_started_at: Option<u64>) -> String {
    match current_turn_started_at {
        Some(started_at) => format_clock(now_epoch_seconds.saturating_sub(started_at)),
        None => "[idle]".into(),
    }
}

/// What a session is doing right now, beyond whether a turn is running.
///
/// The dashboard rows, the chat pane title and the phone all render the same
/// three states from this, so they agree on what "idle" means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionActivity {
    /// When the turn the harness started on its own began, in epoch
    /// milliseconds, while that turn is open. A session with one open is
    /// working even if the projection has not caught up yet.
    pub harness_turn_started_at_ms: Option<i64>,
    /// Commands the agent left running with nothing waiting on them.
    pub background_commands: Vec<crate::hel_worker::BackgroundCommand>,
}

impl SessionActivity {
    /// Read the activity out of what a session's relay last reported.
    pub fn of(operational: &crate::hel_worker::RelayOperationalState) -> Self {
        Self {
            harness_turn_started_at_ms: operational.harness_turn.map(|turn| turn.started_at_ms),
            background_commands: operational.background_commands.clone(),
        }
    }

    /// Epoch seconds the oldest background command started, while the session
    /// has nothing else in flight. Background work is what a session is doing
    /// when it is otherwise idle; during a turn the turn clock says more.
    fn background_since(&self, current_turn_started_at: Option<u64>) -> Option<u64> {
        if current_turn_started_at.is_some() || self.harness_turn_started_at_ms.is_some() {
            return None;
        }
        self.background_commands
            .iter()
            .map(|command| command.started_at_ms)
            .min()
            .and_then(|started_at_ms| u64::try_from(started_at_ms / 1_000).ok())
    }
}

/// The clock columns a wide session row shows: the running turn and its
/// current step, the background work the session left running, or `[idle]`.
pub fn format_activity_columns(
    now_epoch_seconds: u64,
    current_turn_started_at: Option<u64>,
    last_acp_activity_at_ms: Option<u64>,
    activity: &SessionActivity,
) -> Vec<String> {
    if let Some(turn_started) = current_turn_started_at {
        let step_started = last_acp_activity_at_ms
            .map(|value| value / 1_000)
            .unwrap_or(turn_started)
            .max(turn_started);
        return vec![
            format!(
                "Turn {}",
                format_clock(now_epoch_seconds.saturating_sub(turn_started))
            ),
            format!(
                "Step {}",
                format_clock(now_epoch_seconds.saturating_sub(step_started))
            ),
        ];
    }
    match activity.background_since(current_turn_started_at) {
        // The two leading spaces hold the width `Turn` takes, so the clocks
        // stay in one column whichever state a row is in.
        Some(since) => vec![format!(
            "  BG {}",
            format_clock(now_epoch_seconds.saturating_sub(since))
        )],
        None => vec!["[idle]".into()],
    }
}

/// The single-cell form of [`format_activity_columns`], for a narrow row.
pub fn format_activity_clock(
    now_epoch_seconds: u64,
    current_turn_started_at: Option<u64>,
    activity: &SessionActivity,
) -> String {
    match activity.background_since(current_turn_started_at) {
        Some(since) => format!(
            "[BG {}]",
            format_clock(now_epoch_seconds.saturating_sub(since))
        ),
        None => format_turn_clock(now_epoch_seconds, current_turn_started_at),
    }
}

/// Format the dashboard's live-session summary without its trailing session
/// name. The chat transcript uses the same text as its pane title.
pub fn format_session_summary(
    target: &str,
    queued_prompts: usize,
    now_epoch_seconds: u64,
    current_turn_started_at: Option<u64>,
    last_acp_activity_at_ms: Option<u64>,
    activity: &SessionActivity,
    profile: &str,
) -> String {
    let mut columns = vec![target.to_owned()];
    if queued_prompts > 0 {
        columns.push(format!("[Q {queued_prompts}]"));
    }
    columns.extend(format_activity_columns(
        now_epoch_seconds,
        current_turn_started_at,
        last_acp_activity_at_ms,
        activity,
    ));
    columns.push(profile.to_owned());
    columns.join("  ")
}

/// Pure formatter split from local-zone discovery for deterministic tests.
fn format_reset_label(reset: DateTime<FixedOffset>) -> String {
    reset.format("%H:%M %b %-d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_time_normalization_uses_24_hour_month_day_format() {
        let paris = FixedOffset::east_opt(2 * 3_600).expect("offset");
        let reset = paris
            .with_ymd_and_hms(2026, 6, 17, 16, 49, 0)
            .single()
            .expect("instant");
        assert_eq!(format_reset_label(reset), "16:49 Jun 17");
        assert_eq!(
            normalize_reset_text("Jun 17 at 4:49pm").as_deref(),
            Some("16:49 Jun 17")
        );
    }

    #[test]
    fn reset_timestamp_accepts_seconds_and_milliseconds() {
        let seconds = 1_781_712_540_f64;
        assert_eq!(
            format_reset_local(seconds),
            format_reset_local(seconds * 1_000.0)
        );
        assert_eq!(
            format_reset_local(seconds),
            format_reset_local_seconds(seconds as i64)
        );
    }

    #[test]
    fn time_only_reset_is_rendered_as_the_next_datetime() {
        let zone = FixedOffset::west_opt(5 * 3_600).expect("offset");
        let now = zone
            .with_ymd_and_hms(2026, 8, 10, 14, 0, 0)
            .single()
            .expect("now");
        assert_eq!(
            normalize_reset_at("3:30 PM (America/Chicago)", now)
                .map(format_reset_label)
                .as_deref(),
            Some("15:30 Aug 10")
        );
        assert_eq!(
            normalize_reset_at("at 1pm (America/Chicago)", now)
                .map(format_reset_label)
                .as_deref(),
            Some("13:00 Aug 11")
        );
    }

    #[test]
    fn claude_comma_separated_reset_is_normalized() {
        let zone = FixedOffset::west_opt(5 * 3_600).expect("offset");
        let now = zone
            .with_ymd_and_hms(2026, 8, 11, 7, 0, 0)
            .single()
            .expect("now");
        assert_eq!(
            normalize_reset_at("Aug 14, 4am (America/Chicago)", now)
                .map(format_reset_label)
                .as_deref(),
            Some("04:00 Aug 14")
        );
    }

    #[test]
    fn running_clocks_read_as_the_two_largest_units_that_fit() {
        for (seconds, expected) in [
            (0, "0s"),
            (36, "36s"),
            (59, "59s"),
            (60, "1m00s"),
            (2_616, "43m36s"),
            (3_599, "59m59s"),
            (3_600, "1h00m"),
            (6_180, "1h43m"),
            (86_399, "23h59m"),
            (86_400, "1d00h"),
            (183_600, "2d03h"),
        ] {
            assert_eq!(format_clock(seconds), expected, "{seconds} seconds");
        }
    }

    #[test]
    fn turn_clock_formats_running_periods_and_marks_idle_sessions() {
        assert_eq!(format_turn_clock(500, Some(375)), "2m05s");
        assert_eq!(format_turn_clock(400_000, Some(1_000)), "4d14h");
        assert_eq!(format_turn_clock(5_000, None), "[idle]");
    }

    fn background(started_at_ms: i64, command: &str) -> SessionActivity {
        SessionActivity {
            harness_turn_started_at_ms: None,
            background_commands: vec![crate::hel_worker::BackgroundCommand {
                started_at_ms,
                command: command.to_owned(),
            }],
        }
    }

    #[test]
    fn a_session_row_reads_as_its_turn_its_background_work_or_idle() {
        let idle = SessionActivity::default();
        let waiting = background(17_384_000, "cargo test");
        for (label, activity, turn_started, columns, cell) in [
            (
                "running",
                &idle,
                Some(17_384_u64),
                vec!["Turn 43m36s".to_owned(), "Step 12s".to_owned()],
                "43m36s",
            ),
            (
                "background",
                &waiting,
                None,
                vec!["  BG 43m36s".to_owned()],
                "[BG 43m36s]",
            ),
            ("idle", &idle, None, vec!["[idle]".to_owned()], "[idle]"),
        ] {
            assert_eq!(
                format_activity_columns(20_000, turn_started, Some(19_988_000), activity),
                columns,
                "{label} columns"
            );
            assert_eq!(
                format_activity_clock(20_000, turn_started, activity),
                cell,
                "{label} cell"
            );
        }
    }

    #[test]
    fn a_turn_hides_background_work_even_before_the_projection_catches_up() {
        let mut activity = background(17_384_000, "cargo test");
        activity.harness_turn_started_at_ms = Some(19_000_000);

        assert_eq!(
            format_activity_columns(20_000, None, None, &activity),
            vec!["[idle]".to_owned()],
            "a row never claims background work while the harness has a turn open"
        );
        assert_eq!(format_activity_clock(20_000, None, &activity), "[idle]");
    }

    #[test]
    fn session_summary_matches_the_dashboard_without_the_session_name() {
        assert_eq!(
            format_session_summary(
                "precision-3260/bifrost-fuzz",
                0,
                20_000,
                Some(7_847),
                Some(20_000_000),
                &SessionActivity::default(),
                "kimi",
            ),
            "precision-3260/bifrost-fuzz  Turn 3h22m  Step 0s  kimi"
        );
        assert_eq!(
            format_session_summary(
                "morannon",
                2,
                20_000,
                None,
                None,
                &SessionActivity::default(),
                "codex"
            ),
            "morannon  [Q 2]  [idle]  codex"
        );
        assert_eq!(
            format_session_summary(
                "morannon",
                0,
                20_000,
                None,
                None,
                &background(17_384_000, "cargo test"),
                "codex"
            ),
            "morannon    BG 43m36s  codex"
        );
    }
}
