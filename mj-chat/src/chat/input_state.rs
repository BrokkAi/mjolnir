use super::*;

pub(super) const IMAGE_CAPABILITY_NOTICE: &str = "This agent has not advertised image support; paste text or remove image markers before sending";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClipboardTarget {
    Composer,
    Search,
    Answer(String),
    Unavailable,
}

impl ChatState {
    pub fn apply_events(&mut self, events: &[SequencedEvent]) {
        for event in events {
            if event.seq <= self.latest_seq {
                continue;
            }
            self.apply_event(event);
            self.latest_seq = event.seq;
        }
        if !events.is_empty() {
            self.invalidate_render_cache();
        }
    }

    pub(crate) fn reset_interaction(&mut self) {
        self.prompt_history.clear();
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_images.clear();
        self.preferred_column = None;
        self.history_search = None;
        self.queued_prompts.clear();
        self.autocomplete = None;
        self.anchor = TranscriptAnchor::Bottom;
        self.reveal_latest_agent_on_draw = true;
        self.revealed_anchor = None;
        self.last_viewport_height = 0;
        self.render_mode = TranscriptRenderMode::Rich;
        self.transcript_scrollbar.clear();
        if self.feedback.current().is_some() {
            self.feedback.clear();
        }
        self.voice_active = false;
        self.submitting_images.clear();
        self.pending_attachment_markers.clear();
        self.input_generation = self.input_generation.wrapping_add(1);
        self.feedback.clear();
    }

    pub(crate) fn set_input(&mut self, input: String) {
        self.set_input_payload(PromptPayload::text(input));
    }

    pub(crate) fn set_input_payload(&mut self, payload: PromptPayload) {
        self.pending_attachment_markers.clear();
        let mut payload = payload;
        for image in &mut payload.images {
            if image.image.is_pending() {
                image.image = ClipboardImage::failed();
            }
        }
        self.next_image_number = self.next_image_number.max(
            payload
                .images
                .iter()
                .map(|image| image.number.saturating_add(1))
                .max()
                .unwrap_or(1),
        );
        let next_cursor = payload.text.len();
        self.input = payload.text;
        self.input_images = payload.images;
        self.input_cursor = next_cursor;
        self.input_generation = self.input_generation.wrapping_add(1);
        self.feedback.clear();
        self.history_index = None;
        self.preferred_column = None;
        self.update_autocomplete();
    }

    pub(crate) fn clear_input(&mut self) {
        self.set_input(String::new());
    }

    /// Reinstate the input saved when the user last detached, leaving the
    /// cursor at the end. An empty draft leaves the composer alone.
    pub(crate) fn restore_draft(&mut self, draft: String) {
        if draft.is_empty() {
            return;
        }
        match PromptPayload::decode_draft(&draft) {
            Ok(payload) => {
                self.set_input_payload(payload);
                if let Some(body) = draft.strip_prefix(CHAT_DRAFT_PREFIX) {
                    match serde_json::from_str::<SavedChatDraft>(body) {
                        Ok(saved) => {
                            self.unsent_prompts = saved.unsent;
                            self.restore_submissions(saved.pending);
                        }
                        Err(error) => {
                            self.set_notice(format!("Could not restore unsent drafts: {error}"))
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "saved chat draft could not be decoded");
                let fallback = draft
                    .strip_prefix(CHAT_DRAFT_PREFIX)
                    .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
                    .and_then(|value| {
                        value
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                self.set_input(fallback);
                self.set_notice(format!("Saved image draft was discarded: {error}"));
            }
        }
    }

    pub(crate) fn handle_clipboard_content(&mut self, content: ClipboardContent) {
        if self.clipboard_target() == ClipboardTarget::Unavailable {
            return;
        }
        if self.elicitation.is_some() {
            match content {
                ClipboardContent::Text(text) => self.handle_paste(&text),
                ClipboardContent::Image(_) => {
                    self.set_notice("Image paste is unavailable in a text answer field")
                }
            }
            return;
        }
        if self.history_search.is_some() {
            match content {
                ClipboardContent::Text(text) => self.handle_paste(&text),
                ClipboardContent::Image(_) => {
                    self.set_notice("Image paste is unavailable while searching history")
                }
            }
            return;
        }
        match content {
            ClipboardContent::Text(text) => self.handle_paste(&text),
            ClipboardContent::Image(image) => {
                if !self.allow_image_attachment() {
                    return;
                }
                if self.input_images.len() >= MAX_IMAGES {
                    self.set_notice(format!("A prompt can contain at most {MAX_IMAGES} images"));
                    return;
                }
                self.input_cursor = attachments::insert_image(
                    &mut self.input,
                    &mut self.input_images,
                    self.input_cursor,
                    self.next_image_number,
                    image,
                );
                self.next_image_number += 1;
                self.input_generation = self.input_generation.wrapping_add(1);
                self.feedback.clear();
                self.history_index = None;
                self.preferred_column = None;
                self.update_autocomplete();
            }
        }
    }

    pub(crate) fn reserve_attachment(&mut self, sequence: u64) -> bool {
        if !self.allow_image_attachment() {
            return false;
        }
        if self.input_images.len() >= MAX_IMAGES {
            self.set_notice(format!("A prompt can contain at most {MAX_IMAGES} images"));
            return false;
        }
        let number = self.next_image_number;
        self.input_cursor = attachments::insert_image(
            &mut self.input,
            &mut self.input_images,
            self.input_cursor,
            number,
            ClipboardImage::pending(),
        );
        self.next_image_number = self.next_image_number.saturating_add(1);
        self.pending_attachment_markers.insert(sequence, number);
        self.input_generation = self.input_generation.wrapping_add(1);
        self.feedback.clear();

        true
    }

    pub(crate) fn finish_attachment(
        &mut self,
        sequence: u64,
        result: Result<ClipboardImage, String>,
        command: Option<String>,
    ) {
        let Some(number) = self.pending_attachment_markers.remove(&sequence) else {
            // The marker was deleted or the draft was replaced while the
            // blocking task ran. Its result is intentionally discarded.
            return;
        };
        let Some(index) = self
            .input_images
            .iter()
            .position(|image| image.number == number && image.image.is_pending())
        else {
            return;
        };
        match result {
            Ok(ready) => {
                self.input_images[index].image = ready;
                self.input_generation = self.input_generation.wrapping_add(1);
                self.feedback.clear();
            }
            Err(error) => {
                if let Some(command) = command {
                    let range = self.input_images[index].range.clone();
                    let only_placeholder = self.input_images.len() == 1
                        && range.start == 0
                        && range.end == self.input.len();
                    self.replace_input_range(range, &PromptPayload::text(""));
                    if only_placeholder {
                        self.set_input(command);
                    }
                } else {
                    self.input_images[index].image = ClipboardImage::failed();
                    self.input_generation = self.input_generation.wrapping_add(1);
                    self.feedback.clear();
                }
                self.set_notice(format!("Attachment failed: {error}"));
            }
        }
    }

    pub(crate) fn set_prompt_images_supported(&mut self, supported: bool) {
        if self.prompt_images_supported != supported {
            self.prompt_images_supported = supported;
            // Invalidate reads made under the previous capability policy.
            self.input_generation = self.input_generation.wrapping_add(1);
        }
    }

    pub(super) fn clipboard_target(&self) -> ClipboardTarget {
        if self.earlier.is_some()
            || (!self.reviewer_elicitation_open()
                && (self.second_opinion_active() || self.turn_review_active()))
        {
            ClipboardTarget::Unavailable
        } else if let Some(dialog) = &self.elicitation {
            ClipboardTarget::Answer(dialog.request().id.clone())
        } else if self.turn_control_dialog_open
            || self.task_dialog_open
            || self.config_picker_active()
        {
            ClipboardTarget::Unavailable
        } else if self.history_search.is_some() {
            ClipboardTarget::Search
        } else {
            ClipboardTarget::Composer
        }
    }

    pub(crate) fn clipboard_is_text_only(&self) -> bool {
        !self.prompt_images_supported || self.clipboard_target() != ClipboardTarget::Composer
    }

    fn allow_image_attachment(&mut self) -> bool {
        if !self.prompt_images_supported {
            self.set_notice(IMAGE_CAPABILITY_NOTICE);
            return false;
        }
        if self.clipboard_target() != ClipboardTarget::Composer {
            self.set_notice("Image paste is unavailable in this input field");
            return false;
        }
        true
    }

    pub(crate) fn take_submitting_images(&mut self) -> Vec<PromptImage> {
        std::mem::take(&mut self.submitting_images)
    }

    pub(crate) fn draft_payload(&self) -> PromptPayload {
        PromptPayload {
            text: self.input.clone(),
            images: self.input_images.clone(),
        }
    }

    pub(crate) fn input_generation(&self) -> u64 {
        self.input_generation
    }

    pub(crate) fn encoded_draft(&self) -> String {
        if self.unsent_prompts.is_empty() && self.pending_submissions.is_empty() {
            return self.draft_payload().encode_draft();
        }
        let saved = SavedChatDraft {
            composer: self.draft_payload(),
            unsent: self.unsent_prompts.clone(),
            pending: self.pending_submissions.clone(),
        };
        format!(
            "{CHAT_DRAFT_PREFIX}{}",
            serde_json::to_string(&saved).expect("chat draft serialization cannot fail")
        )
    }

    pub(crate) fn restore_latest_unsent_prompt(&mut self) {
        if !self.input.is_empty() {
            self.set_notice("Empty the composer before restoring an unsent prompt");
            return;
        }
        let Some(unsent) = self.unsent_prompts.last().cloned() else {
            self.set_notice("There are no unsent prompts to restore");
            return;
        };
        let mut payload = unsent.payload;
        if unsent.kind == UnsentKind::Shell {
            attachments::replace_range(
                &mut payload.text,
                &mut payload.images,
                0..0,
                &PromptPayload::text("!"),
            );
        }
        self.set_input_payload(payload);
        self.set_notice("Unsent prompt restored · Enter to retry");
    }

    pub(crate) fn edit_latest_queued_prompt(&mut self) -> ChatAction {
        if self
            .queued_prompts
            .back()
            .map(|queued| queued.attachments_unsupported)
            .unwrap_or(false)
        {
            self.set_notice("Queued prompt has unsupported attachments and cannot be edited");
            return ChatAction::None;
        }
        let Some(queued) = self.queued_prompts.pop_back() else {
            return ChatAction::None;
        };
        self.pending_queue_removals.insert(queued.id.clone());
        self.pending_queue_images
            .insert(queued.id.clone(), queued.images.clone());
        self.set_input_payload(PromptPayload {
            text: queued.text.clone(),
            images: queued.images,
        });
        self.set_notice(if queued.kind.is_prompt() {
            "Editing the most recently queued prompt"
        } else {
            "Editing the most recently queued configuration change"
        });
        ChatAction::RemoveQueuedPrompt {
            id: queued.id,
            text: queued.text,
            kind: queued.kind,
        }
    }

    /// Keep a submit the relay refused, so the transcript still shows what was
    /// lost once the notice has gone. Repeating the same failure replaces the
    /// earlier record instead of stacking a second copy of it.
    pub(super) fn record_unsent_prompt(
        &mut self,
        kind: UnsentKind,
        text: String,
        images: Vec<PromptImage>,
        error: String,
    ) {
        self.unsent_prompts.retain(|unsent| {
            unsent.kind != kind || unsent.payload.text != text || unsent.payload.images != images
        });
        self.unsent_prompts.push(UnsentPrompt {
            kind,
            payload: PromptPayload { text, images },
            error,
            recorded_at_ms: mj_core::clock::epoch_millis(),
        });
    }

    /// Drop the record for a submit the relay has now accepted. Nothing else
    /// clears one: a snapshot cannot, because the relay never saw the prompt,
    /// and an unrelated prompt says nothing about this one.
    pub(super) fn clear_unsent_prompt(
        &mut self,
        kind: UnsentKind,
        text: &str,
        images: &[PromptImage],
    ) {
        self.unsent_prompts.retain(|unsent| {
            unsent.kind != kind || unsent.payload.text != text || unsent.payload.images != images
        });
    }

    pub(crate) fn fail_queued_prompt_removal(
        &mut self,
        id: String,
        text: String,
        kind: QueuedCommandKind,
    ) {
        self.pending_queue_removals.remove(&id);
        let images = self.pending_queue_images.remove(&id).unwrap_or_default();
        if !self.queued_prompts.iter().any(|prompt| prompt.id == id) {
            self.queued_prompts.push_back(QueuedPrompt {
                id,
                text,
                kind,
                images,
                attachments_unsupported: false,
            });
        }
    }

    /// The texts of the queued-prompt previews, oldest first.
    pub fn queued_prompt_texts(&self) -> Vec<String> {
        self.queued_prompts
            .iter()
            .map(|queued| queued.text.clone())
            .collect()
    }

    /// Removes the oldest queued-prompt preview with this text and says
    /// whether one was there. Used when a queued prompt could not be handed
    /// to the daemon and its text goes back into the draft.
    pub fn remove_queued_prompt_text(&mut self, text: &str) -> bool {
        let Some(index) = self
            .queued_prompts
            .iter()
            .position(|queued| queued.text == text)
        else {
            return false;
        };
        self.queued_prompts.remove(index);
        true
    }

    pub(crate) fn submit_input(&mut self) -> ChatAction {
        if !self.input_images.is_empty() && !self.prompt_images_supported {
            self.set_notice(IMAGE_CAPABILITY_NOTICE);
            return ChatAction::None;
        }
        let prompt = self.input.trim().to_owned();
        let command_input = if self.input_images.is_empty() {
            prompt.clone()
        } else {
            attachments::text_without_images(&self.input, &self.input_images)
                .trim()
                .to_owned()
        };
        let parsed_command = parse_local_command(&command_input);
        if prompt.is_empty() && self.input_images.is_empty() {
            return ChatAction::None;
        }
        // No session is attached yet. A plain prompt is handed to the host,
        // which asks the daemon to deliver it once the session is live, and
        // stays visible here as a queued preview. Commands need a live
        // session to answer them, so they keep the draft and say so.
        if self.standby {
            if prompt.is_empty() || parsed_command.is_some() || command_input.starts_with('!') {
                self.set_notice("Commands open when the session is live; the draft is kept.");
                return ChatAction::None;
            }
            let id = mj_client::session::new_command_id("standby")
                .unwrap_or_else(|_| format!("standby-{}", self.queued_prompts.len()));
            self.clear_input();
            self.queued_prompts.push_back(QueuedPrompt {
                id,
                text: prompt.clone(),
                kind: QueuedCommandKind::Prompt,
                images: Vec::new(),
                attachments_unsupported: false,
            });
            return ChatAction::Prompt(prompt);
        }
        if self.plan_command_pending
            && !matches!(parsed_command, Some((LocalCommand::GoalControl(_), _)))
        {
            self.set_notice("A plan-mode transition is still in progress");
            return ChatAction::None;
        }
        if !self.input_images.is_empty()
            && (command_input.starts_with('!')
                || parsed_command.is_some_and(|(command, _)| command != LocalCommand::Attach))
        {
            self.set_notice(
                "Send the image with a message, or delete its marker before using a command",
            );
            return ChatAction::None;
        }
        if let Some(command) = prompt.strip_prefix('!') {
            if command.trim().is_empty() {
                self.set_notice("usage: !<bash command>");
                return ChatAction::None;
            }
            if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                self.set_notice("The worker is closing; this shell command was not sent");
                return ChatAction::None;
            }
            self.record_prompt_history(&prompt);
            self.clear_input();
            return ChatAction::RunShell(command.to_owned());
        }
        if let Some((command, args)) = parsed_command {
            return match command {
                LocalCommand::GoalControl(action) => {
                    if !self.goal_state.supports(action) {
                        let mut message = format!(
                            "/goal {} is not supported by this adapter.",
                            action.as_str()
                        );
                        if self
                            .goal_state
                            .supports(mj_core::goal::GoalControlAction::Clear)
                        {
                            message.push_str(" Use /goal clear to remove the goal.");
                        }
                        self.set_notice(message);
                        return ChatAction::None;
                    }
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice("The worker is closing; the goal command was not sent");
                        return ChatAction::None;
                    }
                    self.record_prompt_history(&prompt);
                    self.clear_input();
                    ChatAction::GoalControl { action }
                }
                LocalCommand::Clear => {
                    if !self.clear_context_supported {
                        self.set_notice("This worker does not support /clear; update the worker or start a new session");
                        return ChatAction::None;
                    }
                    if !args.is_empty() {
                        self.set_notice("usage: /clear");
                        return ChatAction::None;
                    }
                    if self.phase != WorkerPhase::Idle || !self.queued_prompts.is_empty() {
                        self.set_notice("/clear requires an idle session with no queued work");
                        return ChatAction::None;
                    }
                    self.submit_prompt(prompt)
                }
                LocalCommand::Help => {
                    self.clear_input();
                    self.show_help();
                    ChatAction::None
                }
                // There is no second screen to return to any more, so
                // /detach now means what the word says: leave Hel with the
                // session still running on its target.
                LocalCommand::Detach => {
                    self.clear_input();
                    ChatAction::QuitDetach
                }
                LocalCommand::Model | LocalCommand::Effort => {
                    let key = if command == LocalCommand::Model {
                        "model"
                    } else {
                        "effort"
                    };
                    if args.is_empty() {
                        if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                            self.set_notice(
                                "The worker is closing; this configuration change was not sent",
                            );
                            return ChatAction::None;
                        }
                        if self.open_config_picker(key) {
                            self.clear_input();
                        } else {
                            self.set_notice(format!(
                                "The agent does not advertise {key} values; usage: /{key} <value>"
                            ));
                        }
                        return ChatAction::None;
                    }
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice(
                            "The worker is closing; this configuration change was not sent",
                        );
                        return ChatAction::None;
                    }
                    // A harness with no selector for this key cannot apply the
                    // change at all. Say so now: sending it would report the
                    // relay's acceptance in the footer and leave the harness's
                    // refusal to arrive in the transcript seconds later.
                    if self.advertised_config_values(key).is_empty() {
                        self.clear_input();
                        self.set_notice(mj_core::acp::missing_config_selector_refusal(key));
                        return ChatAction::None;
                    }
                    // A busy agent does not refuse the change: it waits in the
                    // command queue and applies when its turn comes.
                    self.clear_input();
                    ChatAction::SetConfig {
                        key: key.to_owned(),
                        value: args.to_owned(),
                    }
                }
                LocalCommand::Fast => {
                    if !args.is_empty() {
                        self.set_notice("usage: /fast");
                        return ChatAction::None;
                    }
                    if !self.supports_fast_mode() {
                        self.set_notice("Fast mode is unavailable for the active Codex model");
                        return ChatAction::None;
                    }
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice(
                            "The worker is closing; this configuration change was not sent",
                        );
                        return ChatAction::None;
                    }
                    let value = if self.fast_mode_active() { "off" } else { "on" };
                    self.clear_input();
                    ChatAction::SetConfig {
                        key: "fast-mode".to_owned(),
                        value: value.to_owned(),
                    }
                }
                LocalCommand::Plan => {
                    if self.acp_surface.forwards_plan_command() {
                        return self.submit_prompt_with_history(prompt.clone(), prompt);
                    }
                    let (requested, followup) = match args.to_ascii_lowercase().as_str() {
                        "" => (!self.plan_mode_active(), None),
                        "on" => (true, None),
                        "off" => (false, None),
                        _ => (true, Some(args.to_owned())),
                    };
                    if self.phase != WorkerPhase::Idle {
                        self.set_notice("/plan is only available while the agent is idle");
                        return ChatAction::None;
                    }
                    if requested && self.plan_mode_active() {
                        if let Some(followup) = followup {
                            return self.submit_prompt_with_history(followup, prompt);
                        }
                        self.record_prompt_history(&prompt);
                        self.clear_input();
                        self.set_notice("Plan mode is already on");
                        return ChatAction::None;
                    }
                    if !requested && !self.plan_mode_active() && args.eq_ignore_ascii_case("off") {
                        self.record_prompt_history(&prompt);
                        self.clear_input();
                        self.set_notice("Plan mode is already off");
                        return ChatAction::None;
                    }
                    let control = match self.plan_control(requested) {
                        Ok(control) => control,
                        Err(message) => {
                            self.set_notice(message);
                            return ChatAction::None;
                        }
                    };
                    self.record_prompt_history(&prompt);
                    self.clear_input();
                    self.begin_plan_mode_change(requested);
                    self.plan_command_pending = true;

                    ChatAction::PlanCommand {
                        original: prompt,
                        control,
                        requested_active: requested,
                        prompt: followup,
                    }
                }
                LocalCommand::Review => {
                    self.record_prompt_history(&prompt);
                    self.clear_input();
                    let review = &self.review_config;
                    return match args.trim().to_ascii_lowercase().as_str() {
                        // Bare `/review` reviews the turn that just finished,
                        // whether or not automatic review is armed.
                        "" => ChatAction::StartTurnReview,
                        // Arming is configuration, not a session gesture: a
                        // slash command that edited config.toml would change a
                        // machine-wide setting from inside one conversation.
                        "on" | "off" | "quick" | "extended" => {
                            self.set_notice(
                                "automatic review is configured in config.toml: [review] enabled, tier",
                            );
                            ChatAction::None
                        }
                        "status" => {
                            self.set_notice(review_status_line(review, self.turn_review.is_some()));
                            ChatAction::None
                        }
                        _ => {
                            self.set_notice("usage: /review [status]");
                            ChatAction::None
                        }
                    };
                }
                LocalCommand::Attach => {
                    if !self.allow_image_attachment() {
                        return ChatAction::None;
                    }
                    if args.is_empty() {
                        self.set_notice("usage: /attach <path>");
                        return ChatAction::None;
                    }
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice("The worker is closing; this attachment was not processed");
                        return ChatAction::None;
                    }
                    if self.input_images.len() >= MAX_IMAGES {
                        self.set_notice(format!(
                            "A prompt can contain at most {MAX_IMAGES} images"
                        ));
                        return ChatAction::None;
                    }
                    let path = PathBuf::from(args);
                    let command = command_input.clone();
                    self.record_prompt_history(&command);
                    if self.input_images.is_empty() {
                        self.clear_input();
                    } else if let Some(slash_start) =
                        attachments::untracked_ranges(&self.input, &self.input_images)
                            .into_iter()
                            .find_map(|range| {
                                self.input[range.clone()]
                                    .char_indices()
                                    .find(|(_, character)| !character.is_whitespace())
                                    .map(|(offset, _)| range.start + offset)
                            })
                    {
                        for range in attachments::untracked_ranges(&self.input, &self.input_images)
                            .into_iter()
                            .rev()
                        {
                            let start = range.start.max(slash_start);
                            if start < range.end {
                                self.replace_input_range(
                                    start..range.end,
                                    &PromptPayload::text(""),
                                );
                            }
                        }
                    }
                    ChatAction::Attach { path, command }
                }
                LocalCommand::Implement => {
                    if let Err(message) = self.plan_control(false) {
                        self.set_notice(message);
                        return ChatAction::None;
                    }
                    let instruction = if args.is_empty() {
                        "Implement the approved plan.".to_owned()
                    } else {
                        args.to_owned()
                    };
                    if !self.plan_mode_active() {
                        return self.submit_prompt_with_history(instruction, prompt);
                    }
                    if self.phase != WorkerPhase::Idle {
                        self.set_notice("/implement is only available while the agent is idle");
                        return ChatAction::None;
                    }
                    let control = match self.plan_control(false) {
                        Ok(control) => control,
                        Err(message) => {
                            self.set_notice(message);
                            return ChatAction::None;
                        }
                    };
                    self.record_prompt_history(&prompt);
                    self.clear_input();
                    self.begin_plan_mode_change(false);
                    self.plan_command_pending = true;
                    ChatAction::PlanCommand {
                        original: prompt,
                        control,
                        requested_active: false,
                        prompt: Some(instruction),
                    }
                }
            };
        }
        self.submit_prompt(prompt)
    }

    pub(crate) fn submit_prompt(&mut self, prompt: String) -> ChatAction {
        self.submit_prompt_with_history(prompt.clone(), prompt)
    }

    pub(crate) fn submit_prompt_with_history(
        &mut self,
        prompt: String,
        history: String,
    ) -> ChatAction {
        if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
            self.set_notice("The worker is closing; this prompt was not sent");
            return ChatAction::None;
        }
        // Review is synchronous. While one is unresolved the agent it reviewed
        // stays where the review found it, so findings can never arrive in the
        // middle of the next turn.
        if self.turn_review_active() {
            self.set_notice("A review of the last turn is open; answer it first");
            return ChatAction::None;
        }
        if !history.is_empty() {
            self.record_prompt_history(&history);
        }
        let payload = if self.input_images.is_empty() {
            PromptPayload::text(prompt)
        } else {
            self.draft_payload().trimmed()
        };
        if payload
            .images
            .iter()
            .any(|image| image.image.is_placeholder())
        {
            self.set_notice("Remove failed images or wait for attachments to finish");
            return ChatAction::None;
        }
        self.submitting_images = payload.images;
        self.clear_input();
        ChatAction::Prompt(payload.text)
    }
}
