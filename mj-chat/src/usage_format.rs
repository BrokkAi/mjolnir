//! Shared formatting for live-session, provider quota, and rate-limit displays.

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
    /// Start of the newest pending or in-progress tool call when no explicit
    /// turn is open. This is foreground work the relay can prove from ACP
    /// status, even for a harness that publishes no autonomous-turn boundary.
    pub foreground_tool_started_at_ms: Option<i64>,
    /// Commands the agent left running with nothing waiting on them.
    pub background_commands: Vec<hel::hel_worker::BackgroundCommand>,
}

impl SessionActivity {
    /// Read the activity out of what a session's relay last reported.
    pub fn of(operational: &hel::hel_worker::RelayOperationalState) -> Self {
        Self {
            harness_turn_started_at_ms: operational.harness_turn.map(|turn| turn.started_at_ms),
            foreground_tool_started_at_ms: operational.foreground_tool_started_at_ms,
            background_commands: operational.background_commands.clone(),
        }
    }

    fn foreground_tool_since(&self, current_turn_started_at: Option<u64>) -> Option<u64> {
        if current_turn_started_at.is_some() || self.harness_turn_started_at_ms.is_some() {
            return None;
        }
        self.foreground_tool_started_at_ms
            .and_then(|started_at_ms| u64::try_from(started_at_ms / 1_000).ok())
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
///
/// The step is the tool call, message or thought the agent is on now, which
/// the worker times with [`hel::hel_acp::StepClock`]. A worker too old to
/// report one leaves the step reading as the whole turn rather than
/// pretending to a precision it does not have.
pub fn format_activity_columns(
    now_epoch_seconds: u64,
    current_turn_started_at: Option<u64>,
    current_step_started_at_ms: Option<u64>,
    activity: &SessionActivity,
) -> Vec<String> {
    if let Some(turn_started) = current_turn_started_at {
        let step_started = current_step_started_at_ms
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
    if let Some(since) = activity.foreground_tool_since(current_turn_started_at) {
        return vec![format!(
            "Step {}",
            format_clock(now_epoch_seconds.saturating_sub(since))
        )];
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
    if let Some(since) = activity.foreground_tool_since(current_turn_started_at) {
        return format!(
            "[Step {}]",
            format_clock(now_epoch_seconds.saturating_sub(since))
        );
    }
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
    current_step_started_at_ms: Option<u64>,
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
        current_step_started_at_ms,
        activity,
    ));
    columns.push(profile.to_owned());
    columns.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

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
            foreground_tool_started_at_ms: None,
            background_commands: vec![hel::hel_worker::BackgroundCommand {
                started_at_ms,
                command: command.to_owned(),
            }],
        }
    }

    #[test]
    fn a_foreground_tool_takes_precedence_over_older_background_work() {
        let mut activity = background(17_384_000, "cargo test --old");
        activity.foreground_tool_started_at_ms = Some(19_900_000);

        assert_eq!(
            format_activity_columns(20_000, None, None, &activity),
            vec!["Step 1m40s".to_owned()]
        );
        assert_eq!(
            format_activity_clock(20_000, None, &activity),
            "[Step 1m40s]"
        );
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
    fn a_row_reads_its_step_as_the_whole_turn_when_no_step_is_reported() {
        // A worker too old to time steps sends nothing, and a turn whose
        // first update has not arrived has no step yet. Both read as the
        // turn rather than as a step of zero.
        assert_eq!(
            format_activity_columns(20_000, Some(17_384), None, &SessionActivity::default()),
            vec!["Turn 43m36s".to_owned(), "Step 43m36s".to_owned()]
        );
    }

    #[test]
    fn a_step_that_predates_its_turn_reads_from_the_turn_start() {
        // The step clock belongs to the previous turn until the new turn's
        // first update lands; a row never claims a step older than its turn.
        assert_eq!(
            format_activity_columns(
                20_000,
                Some(19_900),
                Some(17_384_000),
                &SessionActivity::default()
            ),
            vec!["Turn 1m40s".to_owned(), "Step 1m40s".to_owned()]
        );
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
