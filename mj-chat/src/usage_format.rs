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
/// activity facts from this, so they agree on what "idle" means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionActivity {
    pub capacity_retry: Option<hel::hel_worker::CapacityRetry>,
    /// Durable turn start retained through the background work it launched.
    pub activity_turn_started_at_ms: Option<i64>,
    pub prompt_in_flight: bool,
    /// Relay execution state, when this activity came from an operational
    /// snapshot. Materialized-only callers leave it unset.
    pub execution: Option<hel::hel_worker::RelayExecutionState>,
    /// Start of the current observed idle period, when the relay knows it.
    /// Older workers leave this unset even when they report an idle state.
    pub idle_since_ms: Option<i64>,
    /// When the turn the harness started on its own began, in epoch
    /// milliseconds, while that turn is open. A session with one open is
    /// working even if the projection has not caught up yet.
    pub harness_turn_started_at_ms: Option<i64>,
    /// Start of foreground tool work, or a current SDK step while execution
    /// is Running. This also covers harnesses without autonomous-turn markers.
    pub foreground_tool_started_at_ms: Option<i64>,
    /// Commands the agent left running with nothing waiting on them.
    pub background_commands: Vec<hel::hel_worker::BackgroundCommand>,
    /// User shells still owned by the relay. These are separate from agent
    /// background commands, but still make a session non-idle.
    pub active_user_shells: Vec<hel::hel_worker::ActiveUserShell>,
}

impl SessionActivity {
    /// Read the activity out of what a session's relay last reported.
    pub fn of(operational: &hel::hel_worker::RelayOperationalState) -> Self {
        Self {
            capacity_retry: operational.capacity_retry.clone(),
            activity_turn_started_at_ms: operational
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or(operational.activity_turn_started_at_ms),
            prompt_in_flight: operational.active_prompt.is_some(),
            execution: Some(operational.execution),
            idle_since_ms: operational.idle_since_ms,
            harness_turn_started_at_ms: operational.harness_turn.map(|turn| turn.started_at_ms),
            foreground_tool_started_at_ms: operational.foreground_tool_started_at_ms.or_else(
                || {
                    (operational.execution == hel::hel_worker::RelayExecutionState::Running)
                        .then_some(operational.current_step_started_at_ms)
                        .flatten()
                        .filter(|timestamp| *timestamp >= 0)
                },
            ),
            background_commands: operational.background_commands.clone(),
            active_user_shells: operational.active_user_shells.clone(),
        }
    }

    /// Whether the session has no foreground or background work in flight.
    ///
    /// Timestamps are deliberately tested for presence rather than validity:
    /// an invalid timestamp still proves that work exists, and must not make a
    /// row claim that the session is idle. The optional projected turn start is
    /// kept separate because it is supplied by the materialized session rather
    /// than the relay's operational snapshot.
    #[must_use]
    pub fn is_idle(&self, current_turn_started_at: Option<u64>) -> bool {
        self.capacity_retry.is_none()
            && matches!(
                self.kind(current_turn_started_at),
                SessionActivityKind::Idle
            )
    }

    /// Waiting for an answer and queued work are not computation. Background
    /// commands can still run independently while the foreground asks a question.
    pub fn is_working(
        &self,
        current_turn_started_at: Option<u64>,
        waiting_for_input: bool,
    ) -> bool {
        match self.kind(current_turn_started_at) {
            SessionActivityKind::Idle => false,
            SessionActivityKind::Lifecycle => {
                self.execution == Some(hel::hel_worker::RelayExecutionState::Closing)
            }
            SessionActivityKind::Background => true,
            SessionActivityKind::Turn | SessionActivityKind::Step => {
                !waiting_for_input
                    || !self.background_commands.is_empty()
                    || !self.active_user_shells.is_empty()
            }
        }
    }

    /// Compact and detailed clocks use the same evidence as activity indicators.
    pub fn display_clock(
        &self,
        now_epoch_seconds: u64,
        current_turn_started_at: Option<u64>,
        current_step_started_at_ms: Option<u64>,
        detailed: bool,
    ) -> String {
        if let Some(retry) = &self.capacity_retry {
            return retry
                .status(now_epoch_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64);
        }
        let kind = self.kind(current_turn_started_at);
        if kind == SessionActivityKind::Idle {
            return "Idle".into();
        }
        if kind == SessionActivityKind::Lifecycle {
            return self.lifecycle_label().into();
        }
        let turn = current_turn_started_at
            .or_else(|| self.harness_turn_since())
            .or_else(|| self.activity_turn_started_at_ms.and_then(epoch_seconds));
        if detailed {
            return match kind {
                SessionActivityKind::Turn => {
                    let step = current_step_started_at_ms
                        .map(|stamp| stamp / 1_000)
                        .or(turn)
                        .map(|step| turn.map_or(step, |turn| step.max(turn)));
                    format!(
                        "{} {}",
                        elapsed_label("T", now_epoch_seconds, turn),
                        elapsed_label("S", now_epoch_seconds, step)
                    )
                }
                SessionActivityKind::Step => {
                    elapsed_label("S", now_epoch_seconds, self.foreground_tool_since())
                }
                SessionActivityKind::Background => {
                    elapsed_label("BG", now_epoch_seconds, self.background_since())
                }
                _ => unreachable!(),
            };
        }
        let started = match kind {
            SessionActivityKind::Turn => turn,
            SessionActivityKind::Step => turn.or_else(|| self.foreground_tool_since()),
            SessionActivityKind::Background => turn.or_else(|| self.background_since()),
            _ => None,
        };
        elapsed_label("Running", now_epoch_seconds, started)
    }

    /// Classify the current activity and retain the timestamps that support
    /// that classification. This is the structured counterpart to the text
    /// clocks below, so web and terminal surfaces use the same precedence and
    /// never have to parse a rendered activity string.
    #[must_use]
    pub fn details(
        &self,
        current_turn_started_at_ms: Option<i64>,
        current_step_started_at_ms: Option<i64>,
    ) -> SessionActivityDetails {
        let kind = self.kind(current_turn_started_at_ms.map(|_| 0));
        let turn_started_at_ms = match kind {
            SessionActivityKind::Turn => current_turn_started_at_ms
                .or(self.harness_turn_started_at_ms)
                .filter(|timestamp| *timestamp >= 0),
            _ => None,
        };
        let step_started_at_ms = match kind {
            SessionActivityKind::Turn => current_step_started_at_ms
                .filter(|timestamp| *timestamp >= 0)
                .or(turn_started_at_ms)
                .zip(turn_started_at_ms)
                .map(|(step, turn)| step.max(turn)),
            SessionActivityKind::Step => self
                .foreground_tool_started_at_ms
                .filter(|timestamp| *timestamp >= 0),
            _ => None,
        };
        let background_started_at_ms = (kind == SessionActivityKind::Background)
            .then(|| {
                self.background_commands
                    .iter()
                    .map(|command| command.started_at_ms)
                    .chain(
                        self.active_user_shells
                            .iter()
                            .filter_map(|shell| shell.started_at_ms),
                    )
                    .filter(|timestamp| *timestamp >= 0)
                    .min()
            })
            .flatten();
        let label =
            (kind == SessionActivityKind::Lifecycle).then(|| self.lifecycle_label().to_owned());
        SessionActivityDetails {
            kind,
            turn_started_at_ms,
            step_started_at_ms,
            background_started_at_ms,
            idle_since_ms: (kind == SessionActivityKind::Idle)
                .then_some(self.idle_since_ms)
                .flatten()
                .filter(|timestamp| *timestamp >= 0),
            label,
        }
    }

    fn kind(&self, current_turn_started_at: Option<u64>) -> SessionActivityKind {
        match self.execution {
            Some(hel::hel_worker::RelayExecutionState::Closing) => {
                return SessionActivityKind::Lifecycle;
            }
            Some(hel::hel_worker::RelayExecutionState::Closed) => {
                return SessionActivityKind::Lifecycle;
            }
            Some(
                hel::hel_worker::RelayExecutionState::Idle
                | hel::hel_worker::RelayExecutionState::Running,
            )
            | None => {}
        }
        if current_turn_started_at.is_some()
            || self.harness_turn_started_at_ms.is_some()
            || self.prompt_in_flight
        {
            return SessionActivityKind::Turn;
        }
        if self.foreground_tool_started_at_ms.is_some() {
            return SessionActivityKind::Step;
        }
        if !self.background_commands.is_empty() || !self.active_user_shells.is_empty() {
            return SessionActivityKind::Background;
        }
        SessionActivityKind::Idle
    }

    fn lifecycle_label(&self) -> &'static str {
        match self.execution {
            Some(hel::hel_worker::RelayExecutionState::Closing) => "Closing",
            Some(hel::hel_worker::RelayExecutionState::Closed) => "Closed",
            _ => "Lifecycle",
        }
    }

    fn harness_turn_since(&self) -> Option<u64> {
        epoch_seconds(self.harness_turn_started_at_ms?)
    }

    fn foreground_tool_since(&self) -> Option<u64> {
        epoch_seconds(self.foreground_tool_started_at_ms?)
    }

    /// Epoch seconds the oldest background command started. Invalid command
    /// timestamps are ignored only for the clock; their presence still makes
    /// [`Self::is_idle`] false.
    fn background_since(&self) -> Option<u64> {
        self.background_commands
            .iter()
            .filter_map(|command| epoch_seconds(command.started_at_ms))
            .chain(
                self.active_user_shells
                    .iter()
                    .filter_map(|shell| shell.started_at_ms)
                    .filter_map(epoch_seconds),
            )
            .min()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActivityKind {
    Turn,
    Step,
    Background,
    Lifecycle,
    Idle,
}

/// Structured activity facts shared by the terminal and web projections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActivityDetails {
    pub kind: SessionActivityKind,
    pub turn_started_at_ms: Option<i64>,
    pub step_started_at_ms: Option<i64>,
    pub background_started_at_ms: Option<i64>,
    pub idle_since_ms: Option<i64>,
    pub label: Option<String>,
}

fn epoch_seconds(timestamp_ms: i64) -> Option<u64> {
    u64::try_from(timestamp_ms).ok().map(|value| value / 1_000)
}

fn elapsed_label(label: &str, now_epoch_seconds: u64, started_at: Option<u64>) -> String {
    started_at.map_or_else(
        || label.to_owned(),
        |started_at| {
            format!(
                "{label} {}",
                format_clock(now_epoch_seconds.saturating_sub(started_at))
            )
        },
    )
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
    if let Some(retry) = &activity.capacity_retry {
        return vec![
            retry.status(now_epoch_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64),
        ];
    }
    match activity.kind(current_turn_started_at) {
        SessionActivityKind::Turn => {
            let turn_started = current_turn_started_at.or_else(|| activity.harness_turn_since());
            let step_started = current_step_started_at_ms
                .map(|value| value / 1_000)
                .or(turn_started)
                .zip(turn_started)
                .map(|(step, turn)| step.max(turn));
            vec![
                elapsed_label("Turn", now_epoch_seconds, turn_started),
                elapsed_label("Step", now_epoch_seconds, step_started),
            ]
        }
        SessionActivityKind::Step => {
            vec![elapsed_label(
                "Step",
                now_epoch_seconds,
                activity.foreground_tool_since(),
            )]
        }
        SessionActivityKind::Background => {
            // The two leading spaces hold the width `Turn` takes, so the
            // clocks stay in one column whichever state a row is in.
            vec![elapsed_label(
                "  BG",
                now_epoch_seconds,
                activity.background_since(),
            )]
        }
        SessionActivityKind::Lifecycle => vec![activity.lifecycle_label().to_owned()],
        SessionActivityKind::Idle => vec!["[idle]".into()],
    }
}

/// The single-cell form of [`format_activity_columns`], for a narrow row.
pub fn format_activity_clock(
    now_epoch_seconds: u64,
    current_turn_started_at: Option<u64>,
    activity: &SessionActivity,
) -> String {
    if let Some(retry) = &activity.capacity_retry {
        return retry.status(now_epoch_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64);
    }
    match activity.kind(current_turn_started_at) {
        SessionActivityKind::Turn => {
            if current_turn_started_at.is_some() {
                format_turn_clock(now_epoch_seconds, current_turn_started_at)
            } else {
                format!(
                    "[{}]",
                    elapsed_label("Turn", now_epoch_seconds, activity.harness_turn_since())
                )
            }
        }
        SessionActivityKind::Step => format!(
            "[{}]",
            elapsed_label("Step", now_epoch_seconds, activity.foreground_tool_since(),)
        ),
        SessionActivityKind::Background => format!(
            "[{}]",
            elapsed_label("BG", now_epoch_seconds, activity.background_since())
        ),
        SessionActivityKind::Lifecycle => format!("[{}]", activity.lifecycle_label()),
        SessionActivityKind::Idle => "[idle]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_background_agents_render_bg_until_the_live_set_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = hel::hel_worker::DurableRelay::open(
            temp.path(),
            "018f9dd2-a3b4-7c8d-9000-123456789abc",
            "1.0.0",
        )
        .unwrap();
        relay.set_background_work_policy(hel::hel_worker::BackgroundWorkPolicy::ClaudeTasks);
        relay
            .claude_background_tasks_changed(vec![hel::hel_acp::ClaudeBackgroundTask {
                task_id: "design-review".into(),
                description: "Design simplification and cleanup review".into(),
            }])
            .unwrap();
        let state = relay.operational_state();
        let activity = SessionActivity::of(&state);
        let now = (state.background_commands[0].started_at_ms / 1_000) as u64 + 60;
        assert!(!activity.is_idle(None));
        assert_eq!(format_activity_clock(now, None, &activity), "[BG 1m00s]");
        assert_eq!(
            format_activity_columns(now, None, None, &activity),
            vec!["  BG 1m00s"]
        );

        relay.claude_background_tasks_changed(Vec::new()).unwrap();
        let activity = SessionActivity::of(&relay.operational_state());
        assert!(activity.is_idle(None));
        assert_eq!(format_activity_clock(now, None, &activity), "[idle]");
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
    fn a_live_sdk_step_proves_work_but_an_idle_step_clock_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let relay =
            hel::hel_worker::DurableRelay::open(temp.path(), "step-session", "test").unwrap();
        let mut state = relay.operational_state();
        state.execution = hel::hel_worker::RelayExecutionState::Running;
        state.current_step_started_at_ms = Some(50_000);
        let activity = SessionActivity::of(&state);
        assert!(activity.is_working(None, false));
        assert_eq!(activity.display_clock(60, None, None, false), "Running 10s");
        state.execution = hel::hel_worker::RelayExecutionState::Idle;
        assert!(!SessionActivity::of(&state).is_working(None, false));
        state.execution = hel::hel_worker::RelayExecutionState::Running;
        state.current_step_started_at_ms = None;
        assert!(!SessionActivity::of(&state).is_working(None, false));
    }

    #[test]
    fn turn_clock_formats_running_periods_and_marks_idle_sessions() {
        assert_eq!(format_turn_clock(500, Some(375)), "2m05s");
        assert_eq!(format_turn_clock(400_000, Some(1_000)), "4d14h");
        assert_eq!(format_turn_clock(5_000, None), "[idle]");
    }

    fn background(started_at_ms: i64, command: &str) -> SessionActivity {
        SessionActivity {
            capacity_retry: None,
            execution: None,
            activity_turn_started_at_ms: None,
            prompt_in_flight: false,
            idle_since_ms: None,
            harness_turn_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            background_commands: vec![hel::hel_worker::BackgroundCommand {
                id: "test-background".into(),
                started_at_ms,
                command: command.to_owned(),
                can_stop: false,
            }],
            active_user_shells: Vec::new(),
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
    fn structured_activity_clamps_steps_to_their_turn() {
        let activity = SessionActivity::default();
        let details = activity.details(Some(20_000_000), Some(19_000_000));
        assert_eq!(details.kind, SessionActivityKind::Turn);
        assert_eq!(details.turn_started_at_ms, Some(20_000_000));
        assert_eq!(details.step_started_at_ms, Some(20_000_000));

        let details = activity.details(Some(19_000_000), Some(20_000_000));
        assert_eq!(details.step_started_at_ms, Some(20_000_000));
    }

    #[test]
    fn structured_activity_keeps_known_idle_and_missing_idle_since_distinct() {
        let known = SessionActivity {
            execution: Some(hel::hel_worker::RelayExecutionState::Idle),
            idle_since_ms: Some(19_000_000),
            ..SessionActivity::default()
        }
        .details(None, None);
        assert_eq!(known.kind, SessionActivityKind::Idle);
        assert_eq!(known.idle_since_ms, Some(19_000_000));

        let old_worker = SessionActivity {
            execution: Some(hel::hel_worker::RelayExecutionState::Idle),
            ..SessionActivity::default()
        }
        .details(None, None);
        assert_eq!(old_worker.kind, SessionActivityKind::Idle);
        assert_eq!(old_worker.idle_since_ms, None);
    }

    #[test]
    fn a_turn_hides_background_work_even_before_the_projection_catches_up() {
        let mut activity = background(17_384_000, "cargo test");
        activity.harness_turn_started_at_ms = Some(19_000_000);

        assert_eq!(
            format_activity_columns(20_000, None, None, &activity),
            vec!["Turn 16m40s".to_owned(), "Step 16m40s".to_owned()],
            "a row reports the harness turn while the projection catches up"
        );
        assert_eq!(
            format_activity_clock(20_000, None, &activity),
            "[Turn 16m40s]"
        );
    }

    #[test]
    fn idle_requires_no_projected_or_relay_activity() {
        let mut activity = SessionActivity::default();
        assert!(activity.is_idle(None));
        assert!(!activity.is_idle(Some(20_000)));

        activity.harness_turn_started_at_ms = Some(19_000_000);
        assert!(!activity.is_idle(None));

        activity.harness_turn_started_at_ms = None;
        activity.foreground_tool_started_at_ms = Some(19_000_000);
        assert!(!activity.is_idle(None));

        activity.foreground_tool_started_at_ms = None;
        activity.background_commands = vec![hel::hel_worker::BackgroundCommand {
            id: "test-background".into(),
            started_at_ms: 19_000_000,
            command: "cargo test".into(),
            can_stop: false,
        }];
        assert!(!activity.is_idle(None));
    }

    #[test]
    fn invalid_activity_timestamps_still_report_work_without_fabricating_a_clock() {
        let foreground = SessionActivity {
            foreground_tool_started_at_ms: Some(-1),
            ..SessionActivity::default()
        };
        assert!(!foreground.is_idle(None));
        assert_eq!(
            format_activity_columns(20_000, None, None, &foreground),
            vec!["Step".to_owned()]
        );
        assert_eq!(format_activity_clock(20_000, None, &foreground), "[Step]");

        let background = SessionActivity {
            background_commands: vec![hel::hel_worker::BackgroundCommand {
                id: "test-background".into(),
                started_at_ms: -1,
                command: "cargo test".into(),
                can_stop: false,
            }],
            ..SessionActivity::default()
        };
        assert!(!background.is_idle(None));
        assert_eq!(
            format_activity_columns(20_000, None, None, &background),
            vec!["  BG".to_owned()]
        );
        assert_eq!(format_activity_clock(20_000, None, &background), "[BG]");
    }

    #[test]
    fn relay_execution_and_user_shells_keep_activity_non_idle_without_timestamps() {
        let running = SessionActivity {
            prompt_in_flight: true,
            execution: Some(hel::hel_worker::RelayExecutionState::Running),
            ..SessionActivity::default()
        };
        assert!(!running.is_idle(None));
        assert_eq!(
            format_activity_columns(20_000, None, None, &running),
            vec!["Turn".to_owned(), "Step".to_owned()]
        );
        assert_eq!(format_activity_clock(20_000, None, &running), "[Turn]");

        for execution in [
            hel::hel_worker::RelayExecutionState::Closing,
            hel::hel_worker::RelayExecutionState::Closed,
        ] {
            let lifecycle = SessionActivity {
                execution: Some(execution),
                ..SessionActivity::default()
            };
            assert!(!lifecycle.is_idle(None));
            let label = match execution {
                hel::hel_worker::RelayExecutionState::Closing => "Closing",
                hel::hel_worker::RelayExecutionState::Closed => "Closed",
                _ => unreachable!(),
            };
            assert_eq!(
                format_activity_columns(20_000, None, None, &lifecycle),
                vec![label.to_owned()]
            );
            assert_eq!(
                format_activity_clock(20_000, None, &lifecycle),
                format!("[{label}]")
            );
        }

        let shell = SessionActivity {
            active_user_shells: vec![hel::hel_worker::ActiveUserShell {
                command_id: "shell-1".into(),
                command: "cargo test".into(),
                created_at_ms: 20_000_000,
                started_at_ms: None,
            }],
            ..SessionActivity::default()
        };
        assert!(!shell.is_idle(None));
        assert_eq!(
            format_activity_columns(20_000, None, None, &shell),
            vec!["  BG".to_owned()]
        );
        assert_eq!(format_activity_clock(20_000, None, &shell), "[BG]");
    }

    #[test]
    fn compact_clock_continues_the_turn_through_background_work() {
        let mut activity = background(20_000, "build");
        activity.activity_turn_started_at_ms = Some(10_000);
        assert_eq!(
            activity.display_clock(60, Some(10), Some(50_000), false),
            "Running 50s"
        );
        assert_eq!(
            activity.display_clock(60, Some(10), Some(50_000), true),
            "T 50s S 10s"
        );
        assert_eq!(
            activity.display_clock(70, None, None, false),
            "Running 1m00s"
        );
        assert_eq!(activity.display_clock(70, None, None, true), "BG 50s");
        activity.activity_turn_started_at_ms = Some(65_000);
        assert_eq!(
            activity.display_clock(70, Some(65), None, false),
            "Running 5s"
        );
        activity.background_commands.clear();
        assert_eq!(activity.display_clock(70, None, None, false), "Idle");
    }

    #[test]
    fn activity_indicators_ignore_stale_phases_and_questions_without_work() {
        let mut activity = SessionActivity {
            execution: Some(hel::hel_worker::RelayExecutionState::Running),
            ..SessionActivity::default()
        };
        assert!(!activity.is_working(None, false));
        assert_eq!(activity.display_clock(60, None, None, false), "Idle");
        activity.prompt_in_flight = true;
        assert!(activity.is_working(None, false));
        assert_eq!(activity.display_clock(60, None, None, false), "Running");
        assert!(!activity.is_working(Some(10), true));
        activity.background_commands = background(20_000, "build").background_commands;
        assert!(activity.is_working(Some(10), true));
        activity.execution = Some(hel::hel_worker::RelayExecutionState::Closed);
        assert!(!activity.is_working(Some(10), false));
        assert_eq!(activity.display_clock(60, Some(10), None, false), "Closed");
    }
}
