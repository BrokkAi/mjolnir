use super::*;

impl ChatState {
    pub fn handle_key(&mut self, key: KeyEvent) -> ChatAction {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return ChatAction::None;
        }
        // Any key breaks a Ctrl-K chain; only the Ctrl-K arm sets it again.
        let chained = std::mem::take(&mut self.chain_kill);
        let keypad = key.state.contains(KeyEventState::KEYPAD);
        let (code, modifiers) = normalize_key(key.code, key.modifiers);
        if self.earlier.is_some() {
            return self.earlier_key(code);
        }
        if code == KeyCode::PageUp && modifiers.contains(KeyModifiers::CONTROL) {
            self.open_earlier_messages();
            return ChatAction::None;
        }

        // Leaving the view is never an answer to the agent, so these two come
        // before the elicitation dialog. A pending elicitation is durable
        // projection state: it is rebuilt from `pending_elicitations` the next
        // time the session is opened, so stepping out loses nothing but field
        // text that was typed and not submitted.
        // The pane dial, detach, and the web viewer are host bindings behind
        // the prefix key. The host catches them before the composer sees
        // them, so the composer has no escape hatch of its own left.

        // A reviewing harness that asked a question is blocked until it is
        // answered, and its dialog is drawn over the split, so the dialog
        // below takes keys before either review view does. Without this the
        // review's own actions would swallow the answer and the harness would
        // wait for ever.
        let reviewing = !self.reviewer_elicitation_open();

        // The second-opinion view owns the pane while it is up: the composer
        // and the plan decision behind it are both part of what it is deciding.
        if reviewing && self.second_opinion_active() {
            return self.handle_second_opinion_event(key);
        }

        // A turn review owns the pane on the same terms. Its actions are the
        // only input while it is unresolved, which is what holds the primary
        // agent still until the user has answered the findings.
        if reviewing && self.turn_review_active() {
            return self.handle_turn_review_event(key);
        }

        if let Some(dialog) = self.elicitation.as_mut() {
            if code == KeyCode::Char('v')
                && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
            {
                return ChatAction::PasteFromClipboard;
            }
            let request = dialog.request().clone();
            let response = dialog.handle_key_event(key);
            if let Some(response) = response {
                self.elicitation = None;
                if std::mem::take(&mut self.elicitation_is_reviewers) {
                    return ChatAction::RespondReviewerElicitation {
                        role: self.elicitation_role.take(),
                        elicitation_id: request.id,
                        response,
                    };
                }
                // A second opinion is Hel's own decision. Sending it to the
                // harness would consume the plan review before the reviewer
                // exists, so it never becomes an elicitation response.
                if let Some(proposal) =
                    mj_core::acp::plan_review_second_opinion(&request, &response)
                {
                    let proposal = proposal.to_owned();
                    return ChatAction::StartSecondOpinion { request, proposal };
                }
                return ChatAction::RespondElicitation { request, response };
            }
            return ChatAction::None;
        }

        if self.turn_control_dialog_open {
            return self.handle_turn_control_dialog(Event::Key(key));
        }
        if self.task_dialog_open {
            let result =
                self.task_dialog_form
                    .handle(&Event::Key(KeyEvent::new_with_kind_and_state(
                        code, modifiers, key.kind, key.state,
                    )));
            if let Some(Interaction::Activate(control)) = result.action.as_ref() {
                return self.activate_background_task_control(*control);
            }
            if matches!(result.action, Some(Interaction::Cancel)) {
                self.close_task_dialog();
                return ChatAction::None;
            }
            if result.consumed {
                return ChatAction::None;
            }
            match code {
                KeyCode::Up => {
                    let next = self.task_dialog_scroll.saturating_sub(1);
                    self.set_task_dialog_scroll(next);
                }
                KeyCode::Down => {
                    self.set_task_dialog_scroll(self.task_dialog_scroll.saturating_add(1));
                }
                KeyCode::PageUp => {
                    let next = self.task_dialog_scroll.saturating_sub(5);
                    self.set_task_dialog_scroll(next);
                }
                KeyCode::PageDown => {
                    self.set_task_dialog_scroll(self.task_dialog_scroll.saturating_add(5));
                }
                _ => {}
            }
            return ChatAction::None;
        }

        if self.task_control_focused {
            match code {
                KeyCode::Esc | KeyCode::Up => {
                    self.task_control_focused = false;
                }
                KeyCode::Enter => {
                    self.open_task_dialog();
                    return ChatAction::None;
                }
                KeyCode::Right if self.subagent_count > 0 => {
                    self.focus_subagent_control();
                    return ChatAction::None;
                }
                KeyCode::Down => return ChatAction::None,
                _ => {
                    self.task_control_focused = false;
                }
            }
            if code == KeyCode::Esc || code == KeyCode::Up {
                return ChatAction::None;
            }
        }

        if self.subagent_control_focused {
            match code {
                KeyCode::Esc | KeyCode::Up => {
                    self.subagent_control_focused = false;
                }
                KeyCode::Enter => return ChatAction::OpenSubagents,
                KeyCode::Left if self.background_task_count() > 0 => {
                    self.subagent_control_focused = false;
                    self.focus_task_control();
                    return ChatAction::None;
                }
                KeyCode::Down => return ChatAction::None,
                _ => {
                    self.subagent_control_focused = false;
                }
            }
            if code == KeyCode::Esc || code == KeyCode::Up {
                return ChatAction::None;
            }
        }

        // The value selector owns the keyboard while it is up; it is checked
        // after the elicitation dialog because the dialog draws on top of it.
        if self.config_picker_active() {
            return self.handle_config_picker_event(key);
        }

        if code == KeyCode::Char('r')
            && modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.restore_latest_unsent_prompt();
            return ChatAction::None;
        }

        if code == KeyCode::Char('v')
            && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            return ChatAction::PasteFromClipboard;
        }

        if self.history_search.is_some() {
            self.handle_history_search_key(code, modifiers);
            return ChatAction::None;
        }

        if code == KeyCode::Esc {
            if key.kind == KeyEventKind::Repeat
                || (self.turn_control_submitting || self.turn_control_awaiting_state.is_some())
                || self.cancelling_prompt_id.is_some()
            {
                return ChatAction::None;
            }
            if self.steering.as_ref().is_some_and(|s| s.holds_queue()) {
                if self
                    .steering
                    .as_ref()
                    .is_some_and(|s| s.status == mj_core::relay::SteeringStatus::Unconfirmed)
                {
                    self.turn_control_dialog_open = true;
                }
                return ChatAction::None;
            }
            // A prompt of ours, or a turn Claude Code started on its own after
            // a background task, can be cancelled. A Codex goal turn also
            // reads as Running but has its own controls.
            return if self.prompt_in_flight
                || self.harness_turn_stoppable()
                || matches!(
                    self.session_activity.state().last_known(),
                    mj_core::activity::ActivityState::CheckingContinuation
                )
                || (self.session_activity.capacity_retry.is_some()
                    || self.session_activity.quota_recovery.is_some())
                || !self.active_user_shells.is_empty()
            {
                ChatAction::Cancel
            } else {
                ChatAction::None
            };
        }
        // With Num Lock off, the keypad's corner keys are transcript
        // navigation. Enhanced keyboard reporting distinguishes them from the
        // dedicated Home and End keys, which keep editing the prompt line.
        if keypad {
            match code {
                KeyCode::Home => {
                    self.set_anchor(TranscriptAnchor::Row { entry: 0, row: 0 });
                    return ChatAction::None;
                }
                KeyCode::End => {
                    self.set_anchor(TranscriptAnchor::Bottom);
                    return ChatAction::None;
                }
                _ => {}
            }
        }
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                // Reverse-i-search stays on readline's key. The block above
                // hands every key to an open search first, which is what lets
                // Ctrl-R step to the previous match and Alt-R cycle the
                // search's scope once it is open.
                KeyCode::Char('r') => self.begin_history_search(),
                KeyCode::Char('a') => self.move_to_line_start(true),
                KeyCode::Char('e') => self.move_to_line_end(true),
                KeyCode::Char('b') => self.move_input_cursor(-1),
                KeyCode::Char('f') => self.move_input_cursor(1),
                KeyCode::Char('h') => self.backspace(),
                KeyCode::Char('d') => self.delete(),
                KeyCode::Char('u') => self.kill_to_line_start(),
                KeyCode::Char('k') => {
                    self.kill_to_line_end(chained);
                    self.chain_kill = true;
                }
                KeyCode::Char('w') => {
                    let start = text_input::previous_word_start(&self.input, self.input_cursor);
                    self.kill_range(start..self.input_cursor);
                }
                KeyCode::Char('c') => {
                    // Stash the abandoned prompt so history can recall it.
                    if !self.input.is_empty() {
                        let stashed = std::mem::take(&mut self.input);
                        self.record_prompt_history(&stashed);
                        self.clear_input();
                    }
                }
                KeyCode::Char('y') => self.yank(),
                KeyCode::Char('j') | KeyCode::Char('m') => self.insert_character('\n'),
                KeyCode::Char('p') => {
                    if self.input.is_empty() && !self.queued_prompts.is_empty() {
                        return self.edit_latest_queued_prompt();
                    } else if self.input.is_empty() || self.history_index.is_some() {
                        self.move_history(-1);
                    } else {
                        self.move_vertical(-1);
                    }
                }
                KeyCode::Char('n') => {
                    if self.history_index.is_some() {
                        self.move_history(1);
                    } else {
                        self.move_vertical(1);
                    }
                }
                KeyCode::Left => self.move_word(-1),
                KeyCode::Right => self.move_word(1),
                KeyCode::Backspace => {
                    let start = text_input::previous_word_start(&self.input, self.input_cursor);
                    self.kill_range(start..self.input_cursor);
                }
                KeyCode::Delete => {
                    let end = text_input::next_word_end(&self.input, self.input_cursor);
                    self.kill_range(self.input_cursor..end);
                }
                KeyCode::Home => {
                    self.set_anchor(TranscriptAnchor::Row { entry: 0, row: 0 });
                }
                KeyCode::End => self.set_anchor(TranscriptAnchor::Bottom),
                _ => {}
            }
            return ChatAction::None;
        }
        if modifiers.contains(KeyModifiers::ALT) {
            match code {
                KeyCode::Char('b') | KeyCode::Left => self.move_word(-1),
                KeyCode::Char('f') | KeyCode::Right => self.move_word(1),
                KeyCode::Char('d') | KeyCode::Delete => {
                    let end = text_input::next_word_end(&self.input, self.input_cursor);
                    self.kill_range(self.input_cursor..end);
                }
                KeyCode::Backspace => {
                    let start = text_input::previous_word_start(&self.input, self.input_cursor);
                    self.kill_range(start..self.input_cursor);
                }
                KeyCode::Enter => self.insert_character('\n'),
                KeyCode::Up if !self.queued_prompts.is_empty() => {
                    return self.edit_latest_queued_prompt();
                }
                _ => {}
            }
            return ChatAction::None;
        }
        match code {
            KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_character('\n');
                ChatAction::None
            }
            KeyCode::Enter => {
                if self.accept_autocomplete() {
                    ChatAction::None
                } else if self.voice_active {
                    // Voice updates append to the current draft. Keep Enter
                    // from submitting that draft while the recorder still
                    // owns the input, but leave ordinary editing available.
                    ChatAction::None
                } else {
                    self.submit_input()
                }
            }
            KeyCode::Backspace => {
                self.backspace();
                ChatAction::None
            }
            KeyCode::Delete => {
                self.delete();
                ChatAction::None
            }
            // Tab completes an open popup first; with none open it hands the
            // keyboard to the next pane of the combined surface.
            KeyCode::Tab => {
                if self.accept_autocomplete() {
                    ChatAction::None
                } else {
                    ChatAction::CycleFocus { reverse: false }
                }
            }
            KeyCode::BackTab => ChatAction::CycleFocus { reverse: true },
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_character(character);
                ChatAction::None
            }
            KeyCode::Up if self.autocomplete.is_some() => {
                self.move_autocomplete(-1);
                ChatAction::None
            }
            KeyCode::Down if self.autocomplete.is_some() => {
                self.move_autocomplete(1);
                ChatAction::None
            }
            KeyCode::Up => {
                if self.input.is_empty() && !self.queued_prompts.is_empty() {
                    return self.edit_latest_queued_prompt();
                } else if self.input.is_empty() || self.history_index.is_some() {
                    self.move_history(-1);
                } else {
                    self.move_vertical(-1);
                }
                ChatAction::None
            }
            KeyCode::Down => {
                if self.history_index.is_some() {
                    self.move_history(1);
                } else if self.cursor_is_on_last_prompt_line()
                    && (self.background_task_count() > 0 || self.subagent_count > 0)
                {
                    if self.background_task_count() > 0 {
                        self.focus_task_control();
                    } else {
                        self.focus_subagent_control();
                    }
                } else {
                    self.move_vertical(1);
                }
                ChatAction::None
            }
            KeyCode::Left
                if modifiers.contains(KeyModifiers::SHIFT) && !self.queued_prompts.is_empty() =>
            {
                self.edit_latest_queued_prompt()
            }
            KeyCode::Left => {
                self.move_input_cursor(-1);
                ChatAction::None
            }
            KeyCode::Right => {
                self.move_input_cursor(1);
                ChatAction::None
            }
            KeyCode::PageUp => {
                self.scroll_history_up(self.last_viewport_height.max(1));
                ChatAction::None
            }
            KeyCode::PageDown => {
                self.scroll_history_down(self.last_viewport_height.max(1));
                ChatAction::None
            }
            KeyCode::Home => {
                self.move_to_line_start(false);
                ChatAction::None
            }
            KeyCode::End => {
                self.move_to_line_end(false);
                ChatAction::None
            }
            _ => ChatAction::None,
        }
    }
}
