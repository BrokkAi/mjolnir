use super::*;

impl ChatState {
    /// Whether an event belonged to the chat even when its handler had no
    /// action.  This keeps a clamped cursor, an empty composer, and a modal's
    /// no-op key distinguishable from an event the host should route on.
    pub(crate) fn event_consumed(&self, event: &Event, action: &ChatAction) -> bool {
        if !matches!(action, ChatAction::None) {
            return true;
        }
        match event {
            Event::Paste(_) => true,
            Event::Mouse(mouse) => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    return true;
                }
                self.component_handles_mouse(*mouse)
            }
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    return false;
                }
                if self.elicitation.is_some()
                    || self.config_picker_active()
                    || self.task_dialog_open
                    || self.task_control_focused
                    || self.history_search.is_some()
                    || self.second_opinion_active()
                    || self.turn_review_active()
                {
                    return true;
                }
                let ordinary = !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER);
                ordinary
                    && matches!(
                        key.code,
                        KeyCode::Char(_)
                            | KeyCode::Enter
                            | KeyCode::Tab
                            | KeyCode::BackTab
                            | KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::Left
                            | KeyCode::Right
                            | KeyCode::Up
                            | KeyCode::Down
                            | KeyCode::Home
                            | KeyCode::End
                            | KeyCode::PageUp
                            | KeyCode::PageDown
                            | KeyCode::Esc
                    )
            }
            _ => false,
        }
    }

    pub(super) fn set_anchor(&mut self, anchor: TranscriptAnchor) {
        if self.anchor != anchor {
            self.anchor = anchor;
            self.mark_visible_changed();
        }
    }

    pub(crate) fn finish_reviewer_elicitation_response(
        &mut self,
        request: ElicitationRequest,
        response: ElicitationResponse,
    ) -> ChatAction {
        ChatAction::RespondReviewerElicitation {
            role: self.elicitation_role.take(),
            elicitation_id: request.id,
            response,
        }
    }

    pub(crate) fn apply_event(&mut self, event: &SequencedEvent) {
        match &event.event {
            WorkerEvent::PromptAccepted { text, .. } => {
                self.mark_prompt_submitted(text);
                self.start_turn_clock(event.recorded_at_ms);
                self.entries.push(
                    ChatEntry::plain(event.seq, ChatRole::User, text)
                        .with_recorded_at(event.recorded_at_ms),
                );
                self.mark_visible_changed();
            }
            WorkerEvent::TurnCompleted => {
                let changed = self.phase != WorkerPhase::Idle
                    || self.prompt_in_flight
                    || self.session_activity.prompt_in_flight
                    || self.session_activity.harness_turn_started_at_ms.is_some()
                    || self.goal_prompt_active
                    || self.turn_started_at_epoch_seconds.is_some();
                self.phase = WorkerPhase::Idle;
                self.prompt_in_flight = false;
                self.session_activity.prompt_in_flight = false;
                self.session_activity.harness_turn_started_at_ms = None;
                self.goal_prompt_active = false;
                self.turn_started_at_epoch_seconds = None;
                if changed {
                    self.mark_visible_changed();
                }
            }
            // The durable worker records cancellation acceptance before the
            // ACP prompt future resolves. Keep the chat busy until the later
            // TurnCompleted event so a queued prompt cannot race the runtime.
            WorkerEvent::Cancelled => {
                if self.phase != WorkerPhase::Running {
                    self.phase = WorkerPhase::Running;
                    self.mark_visible_changed();
                }
            }
            WorkerEvent::Closing => {
                if self.phase != WorkerPhase::Closing {
                    self.phase = WorkerPhase::Closing;
                    self.mark_visible_changed();
                }
            }
            WorkerEvent::Closed => {
                let changed = self.phase != WorkerPhase::Closed || self.prompt_in_flight;
                self.phase = WorkerPhase::Closed;
                self.prompt_in_flight = false;
                if changed {
                    self.mark_visible_changed();
                }
            }
            WorkerEvent::Checkpointed { .. } => {}
            WorkerEvent::Adapter { payload, .. } => {
                if is_compaction_artifact(payload) {
                    self.last_compaction_seq = event.seq;
                }
                self.apply_adapter(event.seq, event.recorded_at_ms, payload);
            }
            WorkerEvent::QueuedPromptAdded { prompt } => {
                if !self.pending_queue_removals.contains(&prompt.id) {
                    self.queued_prompts.push_back(QueuedPrompt {
                        id: prompt.id.clone(),
                        text: prompt.text.clone(),
                        kind: QueuedCommandKind::Prompt,
                        images: Vec::new(),
                        attachments_unsupported: false,
                    });
                    self.mark_visible_changed();
                }
            }
            WorkerEvent::QueuedPromptRemoved { queue_id } => {
                let before = self.queued_prompts.len();
                self.queued_prompts.retain(|prompt| prompt.id != *queue_id);
                self.pending_queue_removals.remove(queue_id);
                self.pending_queue_images.remove(queue_id);
                if self.queued_prompts.len() != before {
                    self.mark_visible_changed();
                }
            }
            WorkerEvent::QueuedPromptPromoted { prompt, .. } => {
                self.queued_prompts.retain(|queued| queued.id != prompt.id);
                self.pending_queue_removals.remove(&prompt.id);
                self.pending_queue_images.remove(&prompt.id);
                self.phase = WorkerPhase::Running;
                self.prompt_in_flight = true;
                self.start_turn_clock(event.recorded_at_ms);
                self.entries.push(
                    ChatEntry::plain(event.seq, ChatRole::User, &prompt.text)
                        .with_recorded_at(event.recorded_at_ms),
                );
                self.mark_visible_changed();
            }
            WorkerEvent::QueuedPromptsCleared => {
                let changed = !self.queued_prompts.is_empty();
                self.queued_prompts.clear();
                self.pending_queue_removals.clear();
                if changed {
                    self.mark_visible_changed();
                }
            }
            WorkerEvent::ConfigChanged { .. } => {}
        }
    }

    pub(crate) fn apply_adapter(
        &mut self,
        seq: u64,
        recorded_at_ms: Option<i64>,
        payload: &serde_json::Value,
    ) {
        let runtime = match serde_json::from_value::<RuntimeEvent>(payload.clone()) {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::warn!(
                    seq,
                    %error,
                    "ignoring malformed persisted runtime event"
                );
                return;
            }
        };
        let Some(runtime) =
            apply_runtime_event_to_entries(&mut self.entries, seq, recorded_at_ms, runtime)
        else {
            return;
        };
        self.mark_visible_changed();
        match runtime {
            RuntimeEvent::SessionUpdate { update } => {
                self.apply_session_update_at(seq, recorded_at_ms, &update)
            }
            RuntimeEvent::SessionConfigured { config_options } => {
                self.set_config_options(&config_options)
            }
            RuntimeEvent::SessionModesConfigured { modes } => self.set_session_modes(modes),
            _ => {}
        }
    }

    /// Project one typed ACP update into stable transcript items. The runtime
    /// keeps JSON at the persistence boundary so old event logs remain wire
    /// compatible; rendering never guesses at arbitrary JSON shapes.
    #[cfg(test)]
    pub(crate) fn apply_session_update(&mut self, seq: u64, update: &serde_json::Value) {
        self.apply_session_update_at(seq, None, update);
    }

    pub(crate) fn apply_session_update_at(
        &mut self,
        seq: u64,
        recorded_at_ms: Option<i64>,
        update: &serde_json::Value,
    ) {
        let parsed = match serde_json::from_value::<SessionUpdate>(update.clone()) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::debug!(%error, "ignoring invalid ACP session update");
                return;
            }
        };
        match self.goal_state.apply(&parsed) {
            Ok(true) => self.rebuild_command_choices(),
            Ok(false) => {}
            Err(error) => self.set_notice(format!("Could not read goal update: {error:#}")),
        }
        let Some(parsed) =
            apply_session_update_to_entries(&mut self.entries, seq, recorded_at_ms, parsed)
        else {
            return;
        };
        self.mark_visible_changed();
        match parsed {
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.acp_surface
                    .set_agent_commands(update.available_commands);
                self.rebuild_command_choices();
            }
            SessionUpdate::ConfigOptionUpdate(update) => {
                self.set_config_options(&update.config_options);
            }
            SessionUpdate::CurrentModeUpdate(update) => self
                .acp_surface
                .apply_current_mode_update(update.current_mode_id.to_string()),
            _ => {}
        }
    }
}
