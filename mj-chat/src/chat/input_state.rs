use super::attachments::AttachmentFailure;
use super::composer::{PendingMarker, ReadyAttachment};
use super::*;

/// The command name when `text` is shaped like a slash command: a slash
/// followed by a word of letters, digits, `-`, `_`, `:` or `.`. A path such as
/// `/tmp/log is empty` is not a command.
fn slash_command_name(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    let name = rest.split(char::is_whitespace).next()?;
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
    .then_some(name)
}

/// What replaces an attachment's marker when its file cannot be read.
#[derive(Debug)]
pub(crate) enum AttachmentFallback {
    /// Keep the marker, shown as failed: a clipboard image has no text.
    FailedMarker,
    /// Restore the `/attach` command where its marker was.
    Command(String),
    /// Put back the text that was pasted in its place.
    Text(String),
}

pub(super) const IMAGE_PASTE_UNSUPPORTED_NOTICE: &str =
    "This agent does not accept images; only text can be pasted";
pub(super) const IMAGE_CAPABILITY_NOTICE: &str = "This agent has not advertised image support; paste text or remove image markers before sending";

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ClipboardTarget {
    Composer,
    Search,
    Answer(Box<super::ElicitationIdentity>),
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
        self.history_draft_files.clear();
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
        self.submitting_files.clear();
        self.input_generation = self.input_generation.wrapping_add(1);
        self.feedback.clear();
    }

    pub(crate) fn set_input(&mut self, input: String) {
        self.set_input_payload(PromptPayload::text(input));
    }

    pub(crate) fn set_input_payload(&mut self, payload: PromptPayload) {
        self.composer_runtime.submit_when_ready = None;
        self.pending_attachment_markers.clear();
        self.composer_runtime.legacy_draft_images.clear();
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
        self.input_files = payload.files;
        for file in &mut self.input_files {
            if file.file.is_pending() {
                file.file = AttachedFile::failed(&file.file.filename);
            }
        }
        self.advance_file_number();
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

    /// Empties the composer's input.
    pub fn clear_draft(&mut self) {
        self.clear_input();
    }

    /// Reinstate the input saved when the user last detached, leaving the
    /// cursor at the end. An empty draft leaves the composer alone.
    pub(crate) fn restore_draft(&mut self, draft: String) {
        if draft.is_empty() {
            return;
        }
        match PromptPayload::decode_draft(&draft) {
            Ok(payload) => {
                let interrupted_image = payload.images.iter().any(|image| image.image.is_pending());
                let interrupted_file = payload.files.iter().any(|file| file.file.is_pending());
                let legacy = payload
                    .images
                    .iter()
                    .filter(|image| {
                        image.image.reference.is_none() && !image.image.is_placeholder()
                    })
                    .map(|image| (image.number, image.image.clone()))
                    .collect();
                self.set_input_payload(payload);
                self.composer_runtime.legacy_draft_images = legacy;
                if let Some(body) = draft.strip_prefix(CHAT_DRAFT_PREFIX) {
                    match serde_json::from_str::<SavedChatDraft>(body) {
                        Ok(saved) => self.restore_submissions(saved.pending),
                        Err(error) => {
                            self.set_notice(format!("Could not restore pending prompts: {error}"))
                        }
                    }
                }
                if interrupted_file {
                    self.set_notice("A file upload was interrupted. Re-attach the file to retry.");
                } else if interrupted_image {
                    self.set_notice(
                        "An image upload was interrupted. Re-paste the failed attachment to retry.",
                    );
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
                self.set_notice(format!("Saved attachment draft was discarded: {error}"));
            }
        }
    }

    pub(crate) fn handle_clipboard_content(&mut self, content: ClipboardContent) {
        if self.clipboard_target() == ClipboardTarget::Unavailable && self.elicitation.is_none() {
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
                    &mut self.input_files,
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
            &mut self.input_files,
            self.input_cursor,
            number,
            ClipboardImage::pending(),
        );
        self.next_image_number = self.next_image_number.saturating_add(1);
        self.pending_attachment_markers
            .insert(sequence, PendingMarker::Image(number));
        self.input_generation = self.input_generation.wrapping_add(1);
        self.feedback.clear();

        true
    }

    pub(super) fn advance_file_number(&mut self) {
        self.next_file_number = self.next_file_number.max(
            self.input_files
                .iter()
                .map(|file| file.number.saturating_add(1))
                .max()
                .unwrap_or(1),
        );
    }

    pub(super) fn has_input_attachments(&self) -> bool {
        !self.input_images.is_empty() || !self.input_files.is_empty()
    }

    pub(super) fn has_pending_attachments(&self) -> bool {
        self.input_images
            .iter()
            .any(|image| image.image.is_pending())
            || self.input_files.iter().any(|file| file.file.is_pending())
    }

    pub(super) fn reserve_file_attachment(
        &mut self,
        sequence: u64,
        path: &pasted_paths::PastedPath,
    ) -> bool {
        if self.clipboard_target() != ClipboardTarget::Composer {
            self.set_notice("File attachments are unavailable in this input field");
            return false;
        }
        if self.input_files.len() >= mj_core::attachment::MAX_FILES {
            self.set_notice(format!(
                "A prompt can contain at most {} files",
                mj_core::attachment::MAX_FILES
            ));
            return false;
        }
        let mut number = self.next_file_number;
        while self.input_files.iter().any(|file| file.number == number) {
            number = number.wrapping_add(1).max(1);
        }
        self.input_cursor = attachments::insert_file(
            &mut self.input,
            &mut self.input_images,
            &mut self.input_files,
            self.input_cursor,
            number,
            AttachedFile::pending(&path.display_filename()),
        );
        self.next_file_number = number.wrapping_add(1).max(1);
        self.pending_attachment_markers
            .insert(sequence, PendingMarker::File(number));
        self.input_generation = self.input_generation.wrapping_add(1);
        self.feedback.clear();
        true
    }

    pub(super) fn finish_attachment(
        &mut self,
        sequence: u64,
        result: Result<ReadyAttachment, AttachmentFailure>,
        fallback: AttachmentFallback,
    ) {
        let Some(marker) = self.pending_attachment_markers.remove(&sequence) else {
            return;
        };
        let range = match marker {
            PendingMarker::Image(number) => {
                let Some(image) = self
                    .input_images
                    .iter_mut()
                    .find(|image| image.number == number && image.image.is_pending())
                else {
                    return;
                };
                self.composer_runtime.legacy_draft_images.remove(&number);
                if let Ok(ReadyAttachment::Image(ready)) = result {
                    image.image = ready;
                    self.input_generation = self.input_generation.wrapping_add(1);
                    return;
                }
                if matches!(fallback, AttachmentFallback::FailedMarker) {
                    image.image = ClipboardImage::failed();
                    self.input_generation = self.input_generation.wrapping_add(1);
                    self.feedback.clear();
                }
                image.range.clone()
            }
            PendingMarker::File(number) => {
                let Some(file) = self
                    .input_files
                    .iter_mut()
                    .find(|file| file.number == number && file.file.is_pending())
                else {
                    return;
                };
                let range = file.range.clone();
                if let Ok(ReadyAttachment::File(ready)) = result {
                    if file.file.filename == ready.filename {
                        file.file = ready;
                        self.input_generation = self.input_generation.wrapping_add(1);
                    } else {
                        self.replace_attachment_marker(
                            range,
                            &attachments::file_payload(number, ready),
                            true,
                        );
                    }
                    return;
                }
                if matches!(fallback, AttachmentFallback::FailedMarker) {
                    file.file = AttachedFile::failed(&file.file.filename);
                    self.input_generation = self.input_generation.wrapping_add(1);
                    self.feedback.clear();
                }
                range
            }
        };
        let Err(error) = result else {
            tracing::error!(?marker, "attachment result did not match its marker kind");
            return;
        };
        let silent = matches!(
            (&fallback, &error),
            (
                AttachmentFallback::Text(_),
                AttachmentFailure::NotARegularFile(_)
            )
        );
        match fallback {
            AttachmentFallback::Text(text) | AttachmentFallback::Command(text) => {
                self.replace_attachment_marker(range, &PromptPayload::text(text), silent);
            }
            AttachmentFallback::FailedMarker => {}
        }
        if !silent {
            self.set_notice(format!("Attachment failed: {}", error.message()));
        }
    }

    fn replace_attachment_marker(
        &mut self,
        range: std::ops::Range<usize>,
        payload: &PromptPayload,
        preserve_deferred: bool,
    ) {
        let cursor = self.input_cursor;
        let deferred = self.composer_runtime.submit_when_ready.take();
        let feedback = preserve_deferred.then(|| std::mem::take(&mut self.feedback));
        let new_end = range.start + payload.text.len();
        self.replace_input_range(range.clone(), payload);
        if let Some(feedback) = feedback {
            self.composer_runtime.submit_when_ready = deferred;
            self.feedback = feedback;
        }
        self.input_cursor = if cursor >= range.end {
            new_end + (cursor - range.end)
        } else {
            cursor.min(range.start)
        };
        self.update_autocomplete();
    }

    pub(crate) fn set_prompt_images_supported(&mut self, supported: bool) {
        if self.prompt_images_supported != supported {
            self.prompt_images_supported = supported;
            // Invalidate reads made under the previous capability policy.
            self.input_generation = self.input_generation.wrapping_add(1);
        }
    }

    pub(super) fn elicitation_clipboard_target(&self) -> ClipboardTarget {
        self.elicitation_identity()
            .map_or(ClipboardTarget::Unavailable, |identity| {
                ClipboardTarget::Answer(Box::new(identity))
            })
    }

    pub(super) fn clipboard_target(&self) -> ClipboardTarget {
        if self.earlier_reader_has_focus() {
            ClipboardTarget::Unavailable
        } else if self.elicitation.is_some() {
            self.elicitation_clipboard_target()
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
        !(self.standby || self.prompt_images_supported)
            || self.clipboard_target() != ClipboardTarget::Composer
    }

    fn allow_image_attachment(&mut self) -> bool {
        if !(self.standby || self.composer_runtime.accepting_staged || self.prompt_images_supported)
        {
            self.set_notice(IMAGE_CAPABILITY_NOTICE);
            return false;
        }
        if self.clipboard_target() != ClipboardTarget::Composer {
            self.set_notice("Image paste is unavailable in this input field");
            return false;
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn take_submitting_images(&mut self) -> Vec<PromptImage> {
        std::mem::take(&mut self.submitting_images)
    }

    pub(crate) fn draft_payload(&self) -> PromptPayload {
        PromptPayload {
            text: self.input.clone(),
            images: self.input_images.clone(),
            files: self.input_files.clone(),
        }
    }

    pub(crate) fn input_generation(&self) -> u64 {
        self.input_generation
    }

    pub(crate) fn encoded_draft(&self) -> String {
        let mut composer = self.draft_payload();
        // Already-durable legacy bytes stay durable until installation succeeds.
        for image in &mut composer.images {
            if image.image.is_placeholder()
                && let Some(legacy) = self.composer_runtime.legacy_draft_images.get(&image.number)
            {
                image.image = legacy.clone();
            }
        }
        if self.pending_submissions.is_empty() {
            return composer.encode_draft();
        }
        let saved = SavedChatDraft {
            composer,
            pending: self.pending_submissions.clone(),
        };
        format!(
            "{CHAT_DRAFT_PREFIX}{}",
            serde_json::to_string(&saved).expect("chat draft serialization cannot fail")
        )
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
        self.pending_queue_files
            .insert(queued.id.clone(), queued.files.clone());
        self.set_input_payload(PromptPayload {
            text: queued.text.clone(),
            images: queued.images,
            files: queued.files,
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

    pub(crate) fn fail_queued_prompt_removal(
        &mut self,
        id: String,
        text: String,
        kind: QueuedCommandKind,
    ) {
        self.pending_queue_removals.remove(&id);
        let images = self.pending_queue_images.remove(&id).unwrap_or_default();
        let files = self.pending_queue_files.remove(&id).unwrap_or_default();
        if !self.queued_prompts.iter().any(|prompt| prompt.id == id) {
            self.queued_prompts.push_back(QueuedPrompt {
                id,
                text,
                kind,
                images,
                files,
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
        let draft = self.input.clone();
        let command = slash_command_name(draft.trim()).map(str::to_owned);
        let action = self.submit_draft();
        // A slash command the chat refused leaves nothing for the user to
        // edit: keeping it in the draft only makes the next command append
        // to it ("/model/effort high"). Clear it, keep it in history, and
        // name the command in the notice. /clear keeps its own handling, and
        // a draft holding images or waiting for a session is kept.
        let Some(command) = command else {
            return action;
        };
        if !matches!(action, ChatAction::None)
            || command == "clear"
            || self.standby
            || self.has_input_attachments()
            || self.input != draft
        {
            return action;
        }
        let Some(notice) = self.notice() else {
            return action;
        };
        self.record_prompt_history(draft.trim());
        self.clear_input();
        let named = format!("/{command}");
        if notice.contains(&named) {
            self.set_notice(notice);
        } else {
            self.set_notice(format!("{named}: {notice}"));
        }
        action
    }

    fn submit_draft(&mut self) -> ChatAction {
        let prompt = self.input.trim().to_owned();
        let command_input = if !self.has_input_attachments() {
            prompt.clone()
        } else {
            attachments::text_without_attachments(
                &self.input,
                &self.input_images,
                &self.input_files,
            )
            .trim()
            .to_owned()
        };
        let parsed_command = parse_local_command(&command_input);
        let attaching = matches!(parsed_command, Some((LocalCommand::Attach, _)));
        let stage_prompt = self.standby && !attaching;
        if stage_prompt
            && matches!(
                self.composer_runtime.origin,
                super::composer::PromptOrigin::PreparingMove(_)
            )
        {
            self.defer_startup_submission();
            self.set_notice("Preparing the move before queueing the prompt…");
            return ChatAction::None;
        }

        if stage_prompt && self.composer_runtime.paste_in_flight {
            self.defer_startup_submission();
            self.set_notice("Reading clipboard before queueing the prompt…");
            return ChatAction::None;
        }
        if stage_prompt && self.has_pending_attachments() {
            self.defer_startup_submission();
            self.set_notice(
                if self.input_files.iter().any(|file| file.file.is_pending()) {
                    "Preparing files before queueing the prompt…"
                } else {
                    "Preparing images before queueing the prompt…"
                },
            );
            return ChatAction::None;
        }
        if !self.standby
            && !attaching
            && !self.input_images.is_empty()
            && !self.prompt_images_supported
        {
            self.set_notice(IMAGE_CAPABILITY_NOTICE);
            return ChatAction::None;
        }
        if prompt.is_empty() && !self.has_input_attachments() {
            return ChatAction::None;
        }
        // No session is attached yet. A plain prompt is handed to the host,
        // which asks the daemon to deliver it once the session is live, and
        // stays visible here as a queued preview. Commands need a live
        // session to answer them, so they keep the draft and say so.
        if stage_prompt {
            if self
                .input_files
                .iter()
                .any(|file| file.file.is_placeholder())
            {
                self.set_notice("A file attachment failed; remove its marker or attach it again");
                return ChatAction::None;
            }
            if prompt.is_empty() || parsed_command.is_some() || command_input.starts_with('!') {
                self.set_notice("Commands open when the session is live; the draft is kept.");
                return ChatAction::None;
            }
            if self
                .input_images
                .iter()
                .any(|image| image.image.is_placeholder())
            {
                self.set_notice("An image attachment failed; remove its marker or attach it again");
                return ChatAction::None;
            }
            let id = match mj_client::session::new_command_id("standby") {
                Ok(id) => id,
                Err(error) => {
                    self.set_notice(format!("Could not identify the prompt: {error:#}"));
                    return ChatAction::None;
                }
            };
            let origin = match &self.composer_runtime.origin {
                super::composer::PromptOrigin::Move(id) => Some(id.clone()),
                _ => None,
            };
            self.composer_runtime
                .preview_origins
                .insert(id.clone(), origin);
            let payload = self.draft_payload().trimmed();
            self.clear_input();
            self.queued_prompts.push_back(QueuedPrompt {
                id,
                text: payload.text,
                kind: QueuedCommandKind::Prompt,
                images: payload.images,
                files: payload.files,
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
        if self.has_input_attachments()
            && (command_input.starts_with('!')
                || parsed_command.is_some_and(|(command, _)| command != LocalCommand::Attach))
        {
            self.set_notice(if self.input_files.is_empty() {
                "Send the image with a message, or delete its marker before using a command"
            } else {
                "Send the attachment with a message, or delete its marker before using a command"
            });
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
                        self.set_notice(mj_core::acp::missing_config_selector_refusal(
                            self.acp_surface.harness_kind(),
                            key,
                        ));
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
                LocalCommand::Config => {
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice(
                            "The worker is closing; this configuration change was not sent",
                        );
                        return ChatAction::None;
                    }
                    let args = args.trim();
                    if args.is_empty() {
                        if self.open_config_menu() {
                            self.clear_input();
                        } else {
                            self.set_notice("The agent offers no configurable session settings");
                        }
                        return ChatAction::None;
                    }
                    let (key, value) = args
                        .split_once(char::is_whitespace)
                        .map_or((args, None), |(key, value)| (key, Some(value.trim())));
                    if !self.config_key_user_settable(key) {
                        self.clear_input();
                        self.set_notice(self.policy_config_notice());
                        return ChatAction::None;
                    }
                    let values = self.config_values(key);
                    if values.is_empty() {
                        self.clear_input();
                        self.set_notice(format!(
                            "This agent does not advertise {key}; run /config to list its settings"
                        ));
                        return ChatAction::None;
                    }
                    let Some(value) = value.filter(|value| !value.is_empty()) else {
                        if self.open_config_value_dialog(key) {
                            self.clear_input();
                        } else {
                            self.set_notice(format!(
                                "The agent no longer advertises values for {key}"
                            ));
                        }
                        return ChatAction::None;
                    };
                    if !values.iter().any(|choice| choice.value == value) {
                        self.clear_input();
                        self.set_notice(format!(
                            "The agent does not offer {value:?} for {key}; choices: {}",
                            values
                                .iter()
                                .map(|choice| choice.value.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                        return ChatAction::None;
                    }
                    self.clear_input();
                    ChatAction::SetConfig {
                        key: key.to_owned(),
                        value: value.to_owned(),
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
                    let review = self.review_config();
                    return match args.trim().to_ascii_lowercase().as_str() {
                        // Bare `/review` reviews committed work,
                        // whether or not automatic review is armed.
                        "" => ChatAction::StartTurnReview,
                        // Arming is configuration, not a session gesture: a
                        // slash command that edited config.toml would change a
                        // machine-wide setting from inside one conversation.
                        "on" | "off" => {
                            self.set_notice(
                                "review runs before push when reviewers are set in Settings › Code Review",
                            );
                            ChatAction::None
                        }
                        "status" => {
                            if let Some(reason) = self.review_unavailable {
                                self.set_notice(reason);
                            } else {
                                self.set_notice(review_status_line(
                                    &review,
                                    self.has_turn_review(),
                                ));
                            }
                            ChatAction::None
                        }
                        _ => {
                            self.set_notice("usage: /review [status]");
                            ChatAction::None
                        }
                    };
                }
                LocalCommand::Attach => {
                    if args.is_empty() {
                        self.set_notice("usage: /attach <path>");
                        return ChatAction::None;
                    }
                    if matches!(self.phase, WorkerPhase::Closing | WorkerPhase::Closed) {
                        self.set_notice("The worker is closing; this attachment was not processed");
                        return ChatAction::None;
                    }
                    let path = super::pasted_paths::path_argument(args);
                    if self.path_uses_image_mode(&path) {
                        if self.input_images.len() >= MAX_IMAGES {
                            self.set_notice(format!(
                                "A prompt can contain at most {MAX_IMAGES} images"
                            ));
                            return ChatAction::None;
                        }
                    } else if self.input_files.len() >= mj_core::attachment::MAX_FILES {
                        self.set_notice(format!(
                            "A prompt can contain at most {} files",
                            mj_core::attachment::MAX_FILES
                        ));
                        return ChatAction::None;
                    }
                    let command = command_input.clone();
                    self.record_prompt_history(&command);
                    if !self.has_input_attachments() {
                        self.clear_input();
                    } else if let Some(slash_start) = attachments::untracked_ranges(
                        &self.input,
                        &self.input_images,
                        &self.input_files,
                    )
                    .into_iter()
                    .find_map(|range| {
                        self.input[range.clone()]
                            .char_indices()
                            .find(|(_, character)| !character.is_whitespace())
                            .map(|(offset, _)| range.start + offset)
                    }) {
                        for range in attachments::untracked_ranges(
                            &self.input,
                            &self.input_images,
                            &self.input_files,
                        )
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
        if let Some(name) = slash_command_name(&command_input)
            && !self.command_choices.iter().any(|choice| {
                choice.name.eq_ignore_ascii_case(name)
                    || choice
                        .name
                        .split_whitespace()
                        .next()
                        .is_some_and(|first| first.eq_ignore_ascii_case(name))
            })
        {
            self.set_notice(format!(
                "/{name} is not a Mjolnir or agent command; it was not sent"
            ));
            return ChatAction::None;
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
        if !history.is_empty() {
            self.record_prompt_history(&history);
        }
        let payload = if !self.has_input_attachments() {
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
        if payload.files.iter().any(|file| file.file.is_placeholder()) {
            self.set_notice("Remove failed files or wait for attachments to finish");
            return ChatAction::None;
        }
        self.submitting_images = payload.images;
        self.submitting_files = payload.files;
        self.clear_input();
        ChatAction::Prompt(payload.text)
    }
}
