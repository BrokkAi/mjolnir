use super::*;

impl ChatState {
    pub(crate) fn set_active_user_shells(&mut self, shells: &[mj_core::relay::ActiveUserShell]) {
        let next = shells
            .iter()
            .map(|shell| shell.command_id.clone())
            .collect();
        if self.active_user_shells != next {
            self.active_user_shells = next;
        }
    }

    pub(crate) fn set_active_agent_terminals(
        &mut self,
        terminals: &[ActiveAgentTerminal],
        session: &MaterializedSession,
    ) {
        let previous_terminals = &self.active_agent_terminals;
        let previous_claims = &self.claimed_agent_terminals;
        let mut claims = BTreeMap::new();
        let mut unresolved = terminals
            .iter()
            .map(|terminal| (terminal.terminal_id.as_str(), terminal.started_at_ms))
            .collect::<BTreeMap<_, _>>();
        // Claims are normally on the newest item, so walking backward stops
        // immediately. The full-history path is reserved for the uncommon
        // unclaimed fallback this state exists to cover.
        for item in session.transcript.iter().rev() {
            let TranscriptBody::Tool { terminal_refs, .. } = &item.body else {
                continue;
            };
            for terminal_id in terminal_refs {
                let Some(started_at_ms) = unresolved.get(terminal_id.as_str()) else {
                    continue;
                };
                if item.last_changed_at_ms >= *started_at_ms {
                    claims.insert(terminal_id.clone(), item.last_changed_at_ms);
                    unresolved.remove(terminal_id.as_str());
                }
            }
            if unresolved.is_empty() {
                break;
            }
        }
        if previous_terminals != terminals || previous_claims != &claims {
            self.active_agent_terminals = terminals.to_vec();
            self.claimed_agent_terminals = claims;
        }
    }

    pub(crate) fn active_user_shell_ids(&self) -> Vec<String> {
        self.active_user_shells.clone()
    }

    /// Switches the transcript between rendered Markdown and raw source.
    ///
    /// The host owns the key that runs this, so it is called from the
    /// dashboard's command registry as well as from the transcript control.
    pub fn toggle_render_mode(&mut self) {
        self.render_mode = self.render_mode.toggled();
        self.set_notice(match self.render_mode {
            TranscriptRenderMode::Rich => "Rich transcript rendering enabled",
            TranscriptRenderMode::Raw => "Raw transcript source enabled",
        });
    }

    /// What a dictation toggle should do right now.
    ///
    /// A recording remains stoppable even if the helper becomes unavailable
    /// while it is running, so an active recording answers even when voice is
    /// no longer available. An unavailable idle microphone is inert.
    pub fn dictation_toggle_action(&self) -> ChatAction {
        if self.voice_available || self.voice_active {
            ChatAction::ToggleVoice
        } else {
            ChatAction::None
        }
    }

    /// The surfaces the last frame registered, for the selection engine.
    pub fn frame_surfaces(&self) -> &FrameSurfaces {
        &self.frame_surfaces
    }

    /// Whether a shared component should receive this pointer event before
    /// text selection or the host surface. Captured presses remain owned even
    /// after the pointer leaves the control's hitbox.
    pub fn component_handles_mouse(&self, mouse: MouseEvent) -> bool {
        // An elicitation is the visible modal. Check its captured controls
        // before retained task/config/review geometry so those hidden states
        // cannot steal transcript selection or pointer events above it.
        if let Some(dialog) = self.elicitation.as_ref() {
            return dialog.component_handles_mouse_at(mouse.column, mouse.row);
        }
        if self.turn_control_dialog_open {
            return self.turn_control_dialog.captures_pointer()
                || self.turn_control_dialog.contains(mouse.column, mouse.row);
        }
        // The background-task dialog answers for the rectangle it drew,
        // border included, and for nothing else: outside it the pointer
        // belongs to whatever the host drew beside this conversation.
        if self.task_dialog_open
            && (self.task_dialog_form.captures_pointer()
                || self.task_dialog_area.is_some_and(|area| {
                    area.outer(ratatui::layout::Margin::new(1, 1))
                        .contains(Position::new(mouse.column, mouse.row))
                }))
        {
            return true;
        }
        if self
            .task_control_area
            .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
        {
            return true;
        }
        if self
            .subagent_control_area
            .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
        {
            return true;
        }
        if self.voice_form.captures_pointer()
            || self
                .voice_button_area
                .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
        {
            return true;
        }
        if self
            .prompt_config_chip_at(mouse.column, mouse.row)
            .is_some()
        {
            return true;
        }
        if self.config_picker_handles_mouse(mouse.column, mouse.row) {
            return true;
        }
        self.second_opinion_handles_mouse(mouse.column, mouse.row)
            || self.turn_review_handles_mouse(mouse.column, mouse.row)
    }

    /// Whether a dialog of this conversation's own currently stands in for the
    /// ordinary composer.
    pub(super) fn composer_replaced(&self) -> bool {
        self.elicitation.is_some()
            || self.config_picker.is_some()
            || self.task_dialog_open
            || matches!(self.second_opinion, Some(SecondOpinion::Setup { .. }))
    }

    /// Cancels any pointer gesture owned by a chat component.
    pub fn cancel_component_pointer(&mut self) {
        self.task_dialog_form.cancel_pointer();
        self.voice_form.cancel_pointer();
        self.cancel_config_picker_pointer();
        if let Some(dialog) = self.elicitation.as_ref() {
            dialog.cancel_component_pointer();
        }
        self.cancel_second_opinion_pointer();
        self.cancel_turn_review_pointer();
    }

    /// Clears rendered component geometry before a host redraw. Persistent
    /// focus and pointer ownership survive a normal resize; only hitboxes are
    /// invalidated until the next registration pass.
    pub fn reset_component_geometry(&mut self) {
        self.task_dialog_form.reset_geometry();
        self.voice_form.reset_geometry();
        self.voice_button_area = None;
        self.config_chip_areas.clear();
        self.task_control_area = None;
        self.subagent_control_area = None;
        self.task_dialog_area = None;
        self.task_dialog_control_ids.clear();
        self.reset_config_picker_geometry();
        if let Some(dialog) = self.elicitation.as_ref() {
            dialog.reset_component_geometry();
        }
        self.reset_second_opinion_geometry();
        self.reset_turn_review_geometry();
    }

    /// Scrolls the elicitation message pane, for a drag held at its edge.
    pub(crate) fn scroll_elicitation_message(&mut self, rows: isize) {
        if let Some(dialog) = self.elicitation.as_ref() {
            dialog.scroll_message(rows);
        }
    }

    /// The message text a selection in the elicitation pane covers.
    pub fn elicitation_selection_text(&self, range: &SelectionRange) -> Option<String> {
        let dialog = self.elicitation.as_ref()?;
        let width = dialog.message_area()?.width;
        Some(dialog.selection_text(range, width))
    }

    /// Capture is on for every surface, so the app owns the wheel: terminal
    /// scrollback repaints whole TUI frames and is unusably slow on long
    /// sessions.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> ChatAction {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.notices.dismiss(std::time::Instant::now());
        }
        // The topmost form receives the gesture before selection, scrollbars,
        // or a review pane. This also lets reviewer elicitations stay above
        // their split while a stale scrollbar is being redrawn.
        if let Some(dialog) = self.elicitation.as_mut() {
            let over_form = dialog.component_handles_mouse_at(mouse.column, mouse.row);
            let over_message = dialog
                .message_area()
                .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)));
            let over_question_chrome = dialog.rendered_area_contains(mouse.column, mouse.row);
            if over_form || over_message {
                let request = dialog.request().clone();
                let response = dialog.handle_mouse(mouse);
                if let Some(response) = response {
                    self.elicitation = None;
                    if std::mem::take(&mut self.elicitation_is_reviewers) {
                        return self.finish_reviewer_elicitation_response(request, response);
                    }
                    if let Some(proposal) =
                        mj_core::acp::plan_review_second_opinion(&request, &response)
                            .map(str::to_owned)
                    {
                        return ChatAction::StartSecondOpinion { request, proposal };
                    }
                    return ChatAction::RespondElicitation { request, response };
                }
                return ChatAction::None;
            }
            // A transcript thumb can be released after the pointer crosses
            // the question. Preserve that already-captured transcript
            // gesture, while the form-capture branch above still has
            // priority for question controls dragged upward.
            if self.transcript_scrollbar_dragging() && self.handle_transcript_scrollbar_mouse(mouse)
            {
                return ChatAction::None;
            }
            // The question owns its border and footer, but those cells are not
            // controls. Keep a wheel or click there from falling through to
            // the transcript; the upper transcript remains the only region
            // that should scroll while the question is open.
            if over_question_chrome {
                return ChatAction::None;
            }
            // Once a question is visible, stale task/config/review state is
            // retained for later restoration but must not intercept the
            // transcript above it. Route every other mouse event directly to
            // the transcript and stop before those modal handlers below.
            if self.handle_transcript_scrollbar_mouse(mouse) {
                return ChatAction::None;
            }
            let over_transcript = self
                .frame_surfaces
                .surface(crate::selection::SurfaceId::Transcript)
                .is_some_and(|surface| {
                    surface
                        .rect
                        .contains(Position::new(mouse.column, mouse.row))
                });
            if over_transcript {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scroll_history_up(MOUSE_SCROLL_ROWS);
                    }
                    MouseEventKind::ScrollDown => {
                        self.scroll_history_down(MOUSE_SCROLL_ROWS);
                    }
                    _ => {}
                }
            }
            return ChatAction::None;
        }
        if self.turn_control_dialog_open {
            return self.handle_turn_control_dialog(Event::Mouse(mouse));
        }
        if self.task_dialog_open {
            let result = self.task_dialog_form.handle(&Event::Mouse(mouse));
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
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    let next = self.task_dialog_scroll.saturating_sub(3);
                    self.set_task_dialog_scroll(next);
                }
                MouseEventKind::ScrollDown => {
                    self.set_task_dialog_scroll(self.task_dialog_scroll.saturating_add(3));
                }
                MouseEventKind::Down(MouseButton::Left)
                    if self.task_dialog_area.is_some_and(|area| {
                        area.contains(Position::new(mouse.column, mouse.row))
                    }) =>
                {
                    // The list is read-only; clicking it keeps the dialog open.
                }
                _ => {}
            }
            return ChatAction::None;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self
                .task_control_area
                .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
        {
            self.open_task_dialog();
            return ChatAction::None;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self
                .subagent_control_area
                .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
        {
            return ChatAction::OpenSubagents;
        }
        if self.config_picker_active() {
            return self.handle_config_picker_mouse(mouse).1;
        }
        if self.second_opinion_active() && !self.second_opinion_split() {
            let (handled, action) = self.handle_second_opinion_mouse(mouse);
            return if handled { action } else { ChatAction::None };
        }
        if self.handle_transcript_scrollbar_mouse(mouse) {
            return ChatAction::None;
        }
        // A plain transcript click reaches here only after the selection
        // router has confirmed that the press never became a drag. Keeping
        // this after scrollbar hit testing prevents a thumb click from
        // toggling a tool underneath it.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self.toggle_tool_at(mouse.column, mouse.row)
        {
            return ChatAction::None;
        }
        // Hover decides which transcript scrolls while the split is up, so a
        // reviewer answer never moves the reader's place in the primary.
        if self.second_opinion_split() || self.turn_review_split() {
            let turn_review = self.turn_review_split();
            let (component_handled, component_action) = if turn_review {
                self.handle_turn_review_mouse(mouse)
            } else {
                self.handle_second_opinion_mouse(mouse)
            };
            if component_handled {
                return component_action;
            }
            let over_reviewer = self
                .reviewer_area
                .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)));
            let rows = isize::try_from(MOUSE_SCROLL_ROWS).unwrap_or(1);
            match (mouse.kind, over_reviewer) {
                (MouseEventKind::ScrollUp, true) => {
                    if turn_review {
                        self.scroll_turn_review(-rows);
                    } else {
                        self.scroll_second_opinion(-rows);
                    }
                }
                (MouseEventKind::ScrollDown, true) => {
                    if turn_review {
                        self.scroll_turn_review(rows);
                    } else {
                        self.scroll_second_opinion(rows);
                    }
                }
                (MouseEventKind::ScrollUp, false) => {
                    self.scroll_history_up(MOUSE_SCROLL_ROWS);
                }
                (MouseEventKind::ScrollDown, false) => {
                    self.scroll_history_down(MOUSE_SCROLL_ROWS);
                }
                (MouseEventKind::Down(MouseButton::Left), _) => {
                    return if turn_review {
                        self.click_turn_review_action(mouse.column, mouse.row)
                    } else {
                        self.click_split_action(mouse.column, mouse.row)
                    };
                }
                _ => {}
            }
            return ChatAction::None;
        }
        // Setup modals do not enter the split branch above, but they still
        // own the pane and must not expose a stale prompt hitbox from the
        // preceding draw.
        if self.second_opinion_active() || self.turn_review_active() {
            return ChatAction::None;
        }
        // The model and effort chips sit on the prompt border next to the
        // microphone; a click opens that key's value selector.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(key) = self.prompt_config_chip_at(mouse.column, mouse.row)
        {
            self.open_prompt_config_picker(key);
            return ChatAction::None;
        }
        let voice_result = self.voice_form.handle(&Event::Mouse(mouse));
        if let Some(Interaction::Activate(VoiceControl::Microphone)) = voice_result.action {
            return self.dictation_toggle_action();
        }
        if voice_result.consumed {
            return ChatAction::None;
        }
        // Keep the legacy hitbox usable for callers that have not rendered a
        // frame yet. Once the shared form has registered its button, the
        // branch above owns the complete press/release gesture.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self
                .voice_button_area
                .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)))
            && (self.voice_available || self.voice_active)
        {
            return ChatAction::ToggleVoice;
        }
        // The host routes a wheel event here only when the pointer is over
        // the conversation, so it always drives the transcript.
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_history_up(MOUSE_SCROLL_ROWS);
            }
            MouseEventKind::ScrollDown => {
                self.scroll_history_down(MOUSE_SCROLL_ROWS);
            }
            _ => {}
        }
        ChatAction::None
    }
}
