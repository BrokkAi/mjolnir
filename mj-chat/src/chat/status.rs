use super::*;

impl ChatState {
    pub fn phase(&self) -> WorkerPhase {
        self.phase
    }

    /// Records the time-dependent cells represented by the frame just drawn.
    /// The next clock or animation tick can then request a redraw only after
    /// its displayed value actually moves.
    pub fn acknowledge_render(&mut self) {
        self.last_clock_text = Some(self.clock_text(epoch_seconds()));
        self.last_animation_frame = self.needs_animation().then(|| self.activity_spinner());
    }

    pub(crate) fn set_transcript_loading(&mut self, loading: bool) {
        if self.transcript_loading != loading {
            self.transcript_loading = loading;
        }
    }

    pub fn set_history_context(&mut self, bundle_id: impl Into<String>) {
        let bundle_id = bundle_id.into();
        if self.bundle_id.as_deref() != Some(bundle_id.as_str()) {
            self.bundle_id = Some(bundle_id);
        }
    }

    pub fn set_session_modes(&mut self, modes: Option<SessionModeState>) {
        self.acp_surface.set_session_modes(modes);
        self.rebuild_command_choices();
    }

    pub fn set_harness_kind(&mut self, harness_kind: HarnessKind) {
        self.acp_surface.set_harness_kind(harness_kind);
        self.rebuild_command_choices();
    }

    pub(crate) fn supports_plan_mode(&self) -> bool {
        self.acp_surface.supports_plan_mode()
    }

    pub(crate) fn supports_fast_mode(&self) -> bool {
        self.acp_surface.supports_fast_mode()
    }

    pub(crate) fn fast_mode_active(&self) -> bool {
        self.acp_surface.fast_mode_active()
    }

    pub(crate) fn plan_control(&self, active: bool) -> Result<PlanControl, &'static str> {
        self.acp_surface
            .plan_control(active)
            .map_err(plan_control_error_message)
    }

    pub(crate) fn plan_mode_active(&self) -> bool {
        self.acp_surface.plan_mode_active()
    }

    pub(crate) fn begin_plan_mode_change(&mut self, active: bool) {
        self.acp_surface.begin_plan_mode_change(active);
    }

    pub(crate) fn finish_plan_mode_change(&mut self, active: bool) {
        self.acp_surface.finish_plan_mode_change(active);
    }

    #[cfg(test)]
    pub(crate) fn current_mode(&self) -> Option<&str> {
        self.acp_surface.current_mode()
    }

    pub(crate) fn current_model(&self) -> Option<&str> {
        self.acp_surface.current_model()
    }

    pub(crate) fn current_effort(&self) -> Option<&str> {
        self.acp_surface.current_effort()
    }

    pub(super) fn plan_review_followup(
        &self,
        request: &ElicitationRequest,
        response: &ElicitationResponse,
    ) -> Option<PlanReviewFollowup> {
        if !mj_core::acp::is_plan_review_id(&request.id) {
            return None;
        }
        let ElicitationResponse::Accept { content } = response else {
            return Some(PlanReviewFollowup {
                desired_active: true,
                control: None,
                prompt: None,
            });
        };
        let action = match content.get("action") {
            Some(ElicitationValue::String(action)) => action.as_str(),
            _ => "keep_planning",
        };
        let feedback = match content.get("feedback") {
            Some(ElicitationValue::String(feedback)) if !feedback.trim().is_empty() => {
                Some(feedback.clone())
            }
            _ => None,
        };
        Some(match action {
            "implement" => PlanReviewFollowup {
                desired_active: false,
                control: None,
                prompt: None,
            },
            "exit" => PlanReviewFollowup {
                desired_active: false,
                control: self.plan_control(false).ok(),
                prompt: None,
            },
            "revise" => PlanReviewFollowup {
                desired_active: true,
                control: None,
                // Grok carries feedback in its native response. Standard ACP
                // permission responses cannot, so send it as the next planning turn.
                prompt: (!plan_review_carries_native_feedback(&request.id))
                    .then_some(feedback)
                    .flatten(),
            },
            _ => PlanReviewFollowup {
                desired_active: true,
                control: None,
                prompt: None,
            },
        })
    }

    /// Re-points a standby composer at the session that adopted it. The
    /// launch standby is built before the daemon has registered a session, so
    /// it starts with a placeholder id that has to be corrected once the real
    /// id arrives.
    pub fn adopt_session_id(&mut self, session_id: &str) {
        self.session_id = session_id.to_owned();
    }

    /// Installs the stable session-list columns used by the conversation title.
    pub fn set_header_summary(
        &mut self,
        target: impl Into<String>,
        profile: impl Into<String>,
        title: impl Into<String>,
    ) {
        let target = target.into();
        let profile = profile.into();
        let title = title.into();
        if self.header_target != target
            || self.header_profile != profile
            || self.header_title != title
        {
            self.header_target = target;
            self.header_profile = profile;
            self.header_title = title;
        }
    }

    /// The session name the conversation's title shows.
    pub fn header_title(&self) -> &str {
        &self.header_title
    }

    /// Records whether the session has a prompt of ours in flight, which is
    /// what the relay accepts a cancellation for.
    pub(crate) fn set_prompt_in_flight(&mut self, in_flight: bool) {
        if self.prompt_in_flight != in_flight {
            self.prompt_in_flight = in_flight;
        }
        if self.session_activity.prompt_in_flight != in_flight {
            self.session_activity.prompt_in_flight = in_flight;
        }
    }

    #[must_use]
    pub(crate) fn prompt_in_flight(&self) -> bool {
        self.prompt_in_flight
    }

    /// Whether a turn Claude Code started on its own is running, which Stop
    /// interrupts. A Codex turn of this kind is a native goal, which has its
    /// own controls.
    pub(crate) fn harness_turn_stoppable(&self) -> bool {
        self.session_activity.harness_turn_started_at_ms.is_some()
            && !self.session_activity.pursuing_goal
    }

    /// Whether Esc (or the host's Interrupt turn command) has a turn to
    /// interrupt. A prompt of ours, or a turn Claude Code started on its own
    /// after a background task, can be interrupted. A Codex goal turn also
    /// reads as Running but has its own controls.
    pub(crate) fn turn_interruptible(&self) -> bool {
        self.prompt_in_flight
            || self.harness_turn_stoppable()
            || matches!(
                self.session_activity.state().last_known(),
                mj_core::activity::ActivityState::CheckingContinuation
            )
            || (self.session_activity.capacity_retry.is_some()
                || self.session_activity.quota_recovery.is_some())
            || !self.active_user_shells.is_empty()
    }

    /// Whether a turn-control request is already on its way, when a second
    /// interrupt would do nothing.
    pub(crate) fn turn_control_pending(&self) -> bool {
        self.turn_control_submitting
            || self.turn_control_awaiting_state.is_some()
            || self.cancelling_prompt_id.is_some()
            // Another surface, or the worker itself, may be steering.
            || self.steering.as_ref().is_some_and(|s| s.holds_queue())
    }

    pub(super) fn turn_control_intent(&self) -> TurnControlIntent {
        if self.targeted_turn_control_supported
            && self.prompt_in_flight
            && self
                .queued_prompts
                .front()
                .is_some_and(|queued| queued.kind.is_prompt())
        {
            TurnControlIntent::Steer
        } else {
            TurnControlIntent::Cancel
        }
    }

    /// Records what the session is doing beyond its phase, so the pane title
    /// and the composer can name background work.
    pub(crate) fn set_session_activity(
        &mut self,
        activity: mj_client::usage_format::SessionActivity,
    ) {
        // The provider snapshot remains authoritative for task existence. A
        // successful stop acknowledgement does not remove the row itself;
        // only this reconciliation clears its pending label.
        let live_ids = activity
            .background_commands
            .iter()
            .map(|command| command.id.as_str())
            .collect::<BTreeSet<_>>();
        self.pending_background_stops
            .retain(|id| live_ids.contains(id.as_str()));
        if self.session_activity != activity {
            self.session_activity = activity;
        }
        if self.background_task_count() == 0 && self.task_control_focused {
            self.task_control_focused = false;
        }
    }

    #[must_use]
    pub(crate) fn session_activity(&self) -> &mj_client::usage_format::SessionActivity {
        &self.session_activity
    }

    #[must_use]
    pub(crate) fn background_task_count(&self) -> usize {
        self.session_activity.background_commands.len()
    }

    #[must_use]
    pub(crate) fn subagent_count(&self) -> usize {
        self.subagent_count
    }

    pub fn set_subagent_working_count(&mut self, count: usize) {
        self.subagent_working_count = count;
    }

    /// Records whether this session was created with Mjolnir sub-agents, so
    /// the composer can show where they will appear before the first one
    /// exists.
    pub fn set_subagents_enabled(&mut self, enabled: bool) {
        self.subagents_enabled = enabled;
    }

    pub fn set_subagent_count(&mut self, count: usize) {
        if self.subagent_count != count {
            self.subagent_count = count;
            if count == 0 {
                self.subagent_control_focused = false;
            }
        }
    }

    #[must_use]
    pub(crate) fn subagent_control_focused(&self) -> bool {
        self.subagent_control_focused
    }

    pub(crate) fn focus_subagent_control(&mut self) {
        if self.subagent_count > 0 && !self.subagent_control_focused {
            self.task_control_focused = false;
            self.subagent_control_focused = true;
        }
    }

    #[must_use]
    pub(crate) fn background_stop_pending(&self, id: &str) -> bool {
        self.pending_background_stops.contains(id)
    }

    /// Starts a targeted stop request if this task is live, stoppable, and not
    /// already awaiting a provider response. The caller sends the returned
    /// action through the supervised remote worker.
    pub(crate) fn request_background_stop(&mut self, id: String) -> ChatAction {
        let stoppable = self
            .session_activity
            .background_commands
            .iter()
            .any(|command| command.id == id && command.can_stop);
        if stoppable && self.pending_background_stops.insert(id.clone()) {
            ChatAction::StopBackgroundTask { id }
        } else {
            ChatAction::None
        }
    }

    /// Clears a pending stop after the remote provider rejected it. A
    /// successful acknowledgement intentionally has no corresponding call:
    /// the next activity snapshot owns removal and keeps the row honest.
    pub(crate) fn fail_background_stop(&mut self, id: &str, error: &str) {
        if self.pending_background_stops.remove(id) {
            self.set_notice(format!("Background task could not be stopped: {error}"));
        }
    }

    pub(crate) fn fail_all_background_stops(&mut self) -> bool {
        if !self.pending_background_stops.is_empty() {
            self.pending_background_stops.clear();
            true
        } else {
            false
        }
    }

    #[must_use]
    pub(crate) fn task_control_focused(&self) -> bool {
        self.task_control_focused
    }

    #[must_use]
    pub(crate) fn task_dialog_open(&self) -> bool {
        self.task_dialog_open
    }

    pub(crate) fn close_task_dialog(&mut self) {
        if self.task_dialog_open || self.task_dialog_scroll != 0 || self.task_dialog_area.is_some()
        {
            self.task_dialog_open = false;
            self.task_dialog_scroll = 0;
            self.task_dialog_max_scroll = 0;
            self.task_dialog_area = None;
            self.task_dialog_control_ids.clear();
            self.task_dialog_form.clear();
        }
    }

    pub(crate) fn focus_task_control(&mut self) {
        if self.background_task_count() > 0 && !self.task_control_focused {
            self.task_control_focused = true;
        }
    }

    pub(crate) fn open_task_dialog(&mut self) {
        if self.background_task_count() > 0 && !self.task_dialog_open {
            self.task_dialog_open = true;
            self.task_control_focused = false;
            self.task_dialog_scroll = 0;
            self.task_dialog_max_scroll = 0;
        }
    }

    pub(crate) fn set_task_dialog_scroll(&mut self, scroll: usize) {
        let scroll = scroll.min(self.task_dialog_max_scroll);
        if self.task_dialog_scroll != scroll {
            self.task_dialog_scroll = scroll;
        }
    }

    pub(crate) fn activate_background_task_control(
        &mut self,
        control: BackgroundTaskControl,
    ) -> ChatAction {
        self.task_dialog_control_ids
            .iter()
            .find_map(|(candidate, id)| (*candidate == control).then(|| id.clone()))
            .map_or(ChatAction::None, |id| self.request_background_stop(id))
    }

    pub(crate) fn cursor_is_on_last_prompt_line(&self) -> bool {
        let width = self.prompt_content_width.max(1);
        let (_, row) = input::input_cursor_visual_position(&self.input, self.input_cursor, width);
        row.saturating_add(1) >= input::input_visual_rows(&self.input, width)
    }

    pub(crate) fn set_current_step_start(&mut self, timestamp_ms: Option<i64>) {
        let timestamp_ms = timestamp_ms.and_then(|value| u64::try_from(value).ok());
        if self.current_step_started_at_ms != timestamp_ms {
            self.current_step_started_at_ms = timestamp_ms;
        }
    }

    /// The full text of the newest agent message recorded after `seq`.
    ///
    /// A review's context request is answered by the planner's next agent
    /// message, so this is how the answer to a specific request is picked out
    /// of the conversation rather than by taking whatever is last.
    pub(crate) fn latest_agent_text_after(&self, seq: u64) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find(|entry| {
                entry.role == ChatRole::Agent
                    && entry.start_seq > seq
                    && !entry.text.trim().is_empty()
            })
            .map(|entry| entry.text.clone())
    }

    pub fn latest_seq(&self) -> u64 {
        self.latest_seq
    }

    pub fn set_spinner_style(&mut self, style: mj_core::config::SpinnerStyle) {
        if self.spinner_style != style {
            self.spinner_style = style;
        }
    }

    pub fn set_detailed_activity_clocks(&mut self, detailed: bool) {
        if self.detailed_activity_clocks != detailed {
            self.detailed_activity_clocks = detailed;
        }
    }

    /// Activity animates only while the session has work to report.
    pub fn needs_animation(&self) -> bool {
        let primary_working = match self.phase {
            // A closed worker can leave its last operational snapshot in the
            // chat while that snapshot is being reconciled. Its stale primary
            // activity must not keep the spinner alive after the terminal
            // lifecycle event has settled.
            WorkerPhase::Closed => false,
            WorkerPhase::Closing => true,
            WorkerPhase::Running | WorkerPhase::Idle => self.session_activity.is_working(
                self.turn_started_at_epoch_seconds,
                !self.pending_elicitations.is_empty(),
            ),
        };
        (self.activity_reachable && primary_working)
            || self
                .turn_review()
                .is_some_and(|review| review.view.is_working())
    }

    /// Whether the transcript or task clocks differ from the last drawn frame.
    pub fn clock_changed(&self) -> bool {
        self.last_clock_text.as_deref() != Some(self.clock_text(epoch_seconds()).as_str())
    }

    pub(crate) fn clock_text(&self, now: u64) -> String {
        // Reuse the title renderer so review/question/connection precedence
        // cannot diverge from the clock that is actually on screen.
        let mut text = transcript::transcript_title(self, now).to_string();
        if let Some(started_at_ms) = self
            .active_agent_terminals
            .iter()
            .filter(|terminal| {
                self.claimed_agent_terminals
                    .get(&terminal.terminal_id)
                    .is_none_or(|claimed_at_ms| *claimed_at_ms < terminal.started_at_ms)
            })
            .map(|terminal| terminal.started_at_ms)
            .min()
        {
            text.push('\u{1f}');
            text.push_str(&mj_client::usage_format::format_turn_clock(
                now,
                u64::try_from(started_at_ms / 1_000).ok(),
            ));
        }
        if self.task_dialog_open {
            for command in &self.session_activity.background_commands {
                let started =
                    u64::try_from(command.started_at_ms.max(0) / 1_000).unwrap_or_default();
                text.push('\u{1f}');
                text.push_str(&mj_client::usage_format::format_clock(
                    now.saturating_sub(started),
                ));
            }
        }
        text
    }

    /// An animation tick changes the activity spinner while work is visible.
    pub fn animation_changed(&self) -> bool {
        let frame = self.needs_animation().then(|| self.activity_spinner());
        self.last_animation_frame != frame
    }

    pub fn activity_spinner(&self) -> ratatui::text::Line<'static> {
        crate::spinner::activity_line(
            self.spinner_style,
            crate::spinner::elapsed_ms(),
            self.needs_animation(),
        )
    }

    /// Mirrors `[review]` into the view, so `/review status` and the composer
    /// title report what the daemon is actually armed with.
    pub fn set_review_config(&mut self, review: mj_core::config::ReviewConfig) {
        if self.review_config != review {
            self.review_config = review;
        }
    }

    #[must_use]
    pub(crate) fn review_config(&self) -> &mj_core::config::ReviewConfig {
        &self.review_config
    }

    pub(crate) fn mark_prompt_submitted(&mut self, prompt: &str) {
        self.phase = WorkerPhase::Running;
        self.prompt_in_flight = true;
        self.goal_prompt_active = prompt_invokes_command(prompt, "goal");
        self.feedback.clear();
        // Local echo: start the clock now so the header moves with the send.
        // The next materialized update replaces this with the recorded start.
        self.turn_started_at_epoch_seconds = Some(epoch_seconds());
    }

    /// Starts the header clock for a turn the event log just reported. An
    /// event with no recorded time falls back to now, because the turn is
    /// running either way.
    pub(crate) fn start_turn_clock(&mut self, recorded_at_ms: Option<i64>) {
        let started = recorded_at_ms
            .and_then(|recorded_at_ms| u64::try_from(recorded_at_ms).ok())
            .map(|recorded_at_ms| recorded_at_ms / 1_000)
            .or_else(|| Some(epoch_seconds()));
        if self.turn_started_at_epoch_seconds != started {
            self.turn_started_at_epoch_seconds = started;
        }
    }

    pub(crate) fn pursuing_goal(&self) -> bool {
        if self.goal_state.known {
            return self.goal_state.active();
        }
        self.session_activity.pursuing_goal
            || (self.goal_prompt_active && self.acp_surface.advertises_command("goal"))
    }
    pub fn entries(&self) -> &[ChatEntry] {
        &self.entries
    }

    /// Convert a legacy/import transcript projection into the controller's
    /// canonical logical-session model. Native importers use this at their
    /// boundary; live relay sessions are projected directly from relay events.
    pub fn materialized_session(&self) -> MaterializedSession {
        let mut configuration = BTreeMap::new();
        if let Some(model) = self.acp_surface.current_model() {
            configuration.insert("model".into(), serde_json::Value::String(model.to_owned()));
        }
        if let Some(effort) = self.acp_surface.current_effort() {
            configuration.insert(
                "effort".into(),
                serde_json::Value::String(effort.to_owned()),
            );
        }
        mj_transcript::projection::materialized_session_from_entries(
            &self.session_id,
            &self.entries,
            self.latest_seq,
            self.phase,
            configuration,
            self.queued_prompts
                .iter()
                .map(|prompt| MaterializedQueuedPrompt {
                    accepted_ordinal: None,
                    command_id: prompt.id.clone(),
                    kind: prompt.kind.clone(),
                    content: prompt_content_blocks(&prompt.text, &prompt.images),
                    queued_at_ms: 0,
                })
                .collect(),
            self.elicitation
                .as_ref()
                .map(|dialog| vec![dialog.request().clone()])
                .unwrap_or_default(),
        )
    }

    pub fn queued_prompt_snapshot(&self) -> Vec<mj_core::relay::QueuedPrompt> {
        self.queued_prompts
            .iter()
            .map(|prompt| mj_core::relay::QueuedPrompt {
                id: prompt.id.clone(),
                text: prompt.text.clone(),
                attachments: Vec::new(),
                created_at_ms: 0,
            })
            .collect()
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.feedback.set(notice);
    }

    pub(crate) fn clear_notice(&mut self) {
        if self.feedback.current().is_some() {
            self.feedback.clear();
        }
    }

    /// Tells the chat whether the host can start voice dictation. Availability
    /// is separate from [`Self::voice_active`], because an active recording
    /// must remain stoppable if the helper later disappears.
    pub(crate) fn set_voice_available(&mut self, available: bool) {
        if self.voice_available != available {
            self.voice_available = available;
        }
    }

    pub(super) fn set_connection_notice(&mut self, text: impl Into<String>) {
        self.connection_feedback = Some(sanitize_terminal_text(&text.into()));
    }

    /// Feedback local to this conversation, independent of dashboard notices.
    pub fn notice(&self) -> Option<String> {
        self.feedback
            .current()
            .or_else(|| self.connection_feedback.clone())
    }
}
