//! Clipboard and image jobs belong to the live composer, independently of its chat feed.
use super::attachments::AttachmentFailure;
use super::input_state::AttachmentFallback;
use super::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

pub(super) const MAX_ATTACHMENT_TASKS: usize = 2;

#[derive(Debug)]
pub(super) struct AttachmentResult {
    pub(super) sequence: u64,
    pub(super) fallback: AttachmentFallback,
    pub(super) result: Result<ReadyAttachment, AttachmentFailure>,
}

#[derive(Debug)]
pub(super) enum AttachmentSource {
    Clipboard(ClipboardImage),
    ImagePath(super::pasted_paths::PastedPath),
    FilePath(super::pasted_paths::PastedPath),
}

#[derive(Debug)]
pub(super) enum ReadyAttachment {
    Image(ClipboardImage),
    File(AttachedFile),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingMarker {
    Image(u64),
    File(u64),
}

#[derive(Debug)]
pub(super) enum ComposerIoUpdate {
    Clipboard {
        generation: u64,
        target: input_state::ClipboardTarget,
        staging: bool,
        result: Result<ClipboardContent, String>,
    },
    SessionHistoryListed {
        generation: u64,
        result: Result<Vec<mj_core::storage::PromptHistoryEntry>, String>,
    },
    HistorySearchResults {
        generation: u64,
        result: Result<mj_core::storage::PromptHistoryPage, String>,
    },
    AttachmentFinished(AttachmentResult),
    ReceiptPrepared {
        command_id: String,
        result: Result<PromptPayload, String>,
    },
    Remote(remote::ChatRemoteResult),
}

pub(super) struct RemoteResultForwarder(pub(super) Option<tokio::task::JoinHandle<()>>);
impl RemoteResultForwarder {
    fn is_finished(&self) -> bool {
        self.0
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
    }
}
impl Drop for RemoteResultForwarder {
    fn drop(&mut self) {
        let Some(task) = self.0.take() else {
            return;
        };
        if !task.is_finished() {
            task.abort();
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Err(error) = task.await
                        && !error.is_cancelled()
                    {
                        tracing::error!(%error, "native result forwarding task failed");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error,"could not supervise native result forwarding shutdown")
            }
        }
    }
}

#[derive(Clone, Default)]
pub(super) enum PromptOrigin {
    #[default]
    Ordinary,
    PreparingMove(u64),
    Move(String),
}
#[derive(Clone)]
pub(super) struct DeferredSubmission {
    pub(super) origin: PromptOrigin,
}

pub(super) struct ComposerRuntime {
    pub(super) tx: UnboundedSender<ComposerIoUpdate>,
    pub(super) rx: UnboundedReceiver<ComposerIoUpdate>,
    pub(super) paste_in_flight: bool,
    pub(super) receipt_preparations: BTreeMap<String, RemoteResultForwarder>,
    pub(super) prepared_receipt_retries: VecDeque<String>,
    pub(super) legacy_draft_images: BTreeMap<u64, ClipboardImage>,
    pub(super) attachment_queue: VecDeque<(u64, AttachmentSource, AttachmentFallback)>,
    pub(super) attachment_tasks_in_flight: usize,
    pub(super) next_attachment_sequence: u64,
    attachment_cancellations: BTreeMap<u64, tokio::sync::oneshot::Sender<()>>,
    session_id: Option<String>,
    pub(super) accepting_staged: bool,
    pub(super) submit_when_ready: Option<DeferredSubmission>,
    pub(super) origin: PromptOrigin,
    pub(super) preview_origins: BTreeMap<String, Option<String>>,
    pub(super) prepared_startup: VecDeque<StartupPrompt>,
    pub(super) remote_streams: Vec<RemoteResultForwarder>,
    pub(super) history_task: Option<RemoteResultForwarder>,
    pub(super) session_history_task: Option<RemoteResultForwarder>,
}
impl Default for ComposerRuntime {
    fn default() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            tx,
            rx,
            paste_in_flight: false,
            receipt_preparations: BTreeMap::new(),
            prepared_receipt_retries: VecDeque::new(),
            legacy_draft_images: BTreeMap::new(),
            attachment_queue: VecDeque::new(),
            attachment_tasks_in_flight: 0,
            next_attachment_sequence: 0,
            attachment_cancellations: BTreeMap::new(),
            session_id: None,
            accepting_staged: false,
            submit_when_ready: None,
            origin: PromptOrigin::Ordinary,
            preview_origins: BTreeMap::new(),
            prepared_startup: VecDeque::new(),
            remote_streams: Vec::new(),
            history_task: None,
            session_history_task: None,
        }
    }
}

/// A queued startup prompt retains its stable identity and image-preserving draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupPrompt {
    pub command_id: String,
    pub payload: PromptPayload,
    pub move_operation_id: Option<String>,
}

/// In-memory handoff only: pending jobs and their destination travel with the draft.
pub struct LiveComposer {
    input: String,
    input_cursor: usize,
    input_images: Vec<PromptImage>,
    input_files: Vec<PromptFile>,
    next_image_number: u64,
    next_file_number: u64,
    submitting_images: Vec<PromptImage>,
    submitting_files: Vec<PromptFile>,
    pending_attachment_markers: BTreeMap<u64, PendingMarker>,
    input_generation: u64,
    prompt_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    history_draft_images: Vec<PromptImage>,
    history_draft_files: Vec<PromptFile>,
    kill_buffer: String,
    kill_images: Vec<PromptImage>,
    kill_files: Vec<PromptFile>,
    chain_kill: bool,
    preferred_column: Option<usize>,
    queued_prompts: VecDeque<super::QueuedPrompt>,
    pending_queue_removals: BTreeSet<String>,
    pending_queue_images: BTreeMap<String, Vec<PromptImage>>,
    pending_queue_files: BTreeMap<String, Vec<PromptFile>>,
    history_search: Option<HistorySearch>,
    next_history_search_generation: u64,
    composer_requests: VecDeque<super::history::ComposerRequest>,
    session_history: Vec<String>,
    session_history_error: Option<String>,
    session_history_generation: u64,
    pending_submissions: Vec<submissions::PendingSubmission>,
    submission_renders: Vec<(String, std::time::Instant)>,
    plan_command_pending: bool,
    operation_feedback: BTreeMap<String, String>,
    deferred_form_results: Vec<remote::ChatRemoteResult>,
    conversation_notices: Vec<feedback::ConversationNotice>,
    pub(super) composer_runtime: ComposerRuntime,
}

impl ChatState {
    /// Recover refused content without resetting newer pending image jobs.
    pub fn restore_startup_prompt(&mut self, prompt: StartupPrompt) -> anyhow::Result<()> {
        let merged = mj_core::draft::merge_drafts(&self.draft(), &prompt.payload.encode_draft())?;
        let payload = PromptPayload::decode_draft(&merged)?;
        let was_empty = self.input.is_empty();
        self.input = payload.text;
        self.input_images = payload.images;
        self.input_files = payload.files;
        self.advance_file_number();
        if was_empty {
            self.input_cursor = self.input.len();
        }
        self.next_image_number = self.next_image_number.max(
            self.input_images
                .iter()
                .map(|image| image.number.saturating_add(1))
                .max()
                .unwrap_or(1),
        );
        self.input_generation = self.input_generation.wrapping_add(1);
        self.composer_runtime.submit_when_ready = None;
        self.queued_prompts
            .retain(|queued| queued.id != prompt.command_id);
        self.settle_startup_receipt(&prompt.command_id);
        self.update_autocomplete();
        Ok(())
    }
    pub fn settle_startup_preview(&mut self, command_id: &str) {
        self.settle_startup_receipt(command_id);
        self.queued_prompts.retain(|queued| queued.id != command_id);
        self.pending_queue_removals.remove(command_id);
        self.pending_queue_images.remove(command_id);
        self.pending_queue_files.remove(command_id);
        self.composer_runtime.preview_origins.remove(command_id);
    }
    pub fn recalled_startup_prompt(&self, command_id: &str, text: &str) -> StartupPrompt {
        StartupPrompt {
            command_id: command_id.to_owned(),
            move_operation_id: self
                .composer_runtime
                .preview_origins
                .get(command_id)
                .cloned()
                .flatten(),
            payload: PromptPayload {
                text: text.to_owned(),
                images: self
                    .pending_queue_images
                    .get(command_id)
                    .cloned()
                    .unwrap_or_default(),
                files: self
                    .pending_queue_files
                    .get(command_id)
                    .cloned()
                    .unwrap_or_default(),
            },
        }
    }
    pub fn startup_prompt_withdrawn(
        &mut self,
        prompt: StartupPrompt,
        result: Result<bool, String>,
    ) -> anyhow::Result<bool> {
        self.pending_queue_removals.remove(&prompt.command_id);
        self.pending_queue_images.remove(&prompt.command_id);
        self.pending_queue_files.remove(&prompt.command_id);
        match result {
            Ok(true) => Ok(false),
            Ok(false) => {
                if self.draft_payload() == prompt.payload {
                    self.clear_input();
                    return Ok(true);
                }
                Ok(false)
            }
            Err(error) => {
                if self.draft_payload() == prompt.payload {
                    self.clear_input();
                }
                if !self
                    .queued_prompts
                    .iter()
                    .any(|queued| queued.id == prompt.command_id)
                {
                    self.queued_prompts.push_back(super::QueuedPrompt {
                        id: prompt.command_id,
                        text: prompt.payload.text,
                        images: prompt.payload.images,
                        files: prompt.payload.files,
                        kind: QueuedCommandKind::Prompt,
                        attachments_unsupported: false,
                    });
                }
                anyhow::bail!(error)
            }
        }
    }
    pub fn startup_prompts(&self) -> Vec<StartupPrompt> {
        self.queued_prompts
            .iter()
            .map(|queued| StartupPrompt {
                command_id: queued.id.clone(),
                move_operation_id: self
                    .composer_runtime
                    .preview_origins
                    .get(&queued.id)
                    .cloned()
                    .flatten(),
                payload: PromptPayload {
                    text: queued.text.clone(),
                    images: queued.images.clone(),
                    files: queued.files.clone(),
                },
            })
            .collect()
    }
    pub fn latest_startup_prompt(&self) -> Option<StartupPrompt> {
        self.startup_prompts().pop()
    }

    /// A parked worker view forwards results to the composer which took its state.
    pub fn has_live_composer(&self) -> bool {
        self.composer_forward.is_none()
    }
    pub fn take_live_composer(&mut self) -> Option<LiveComposer> {
        if !self.has_live_composer() {
            return None;
        }
        self.composer_forward = Some(self.composer_runtime.tx.clone());
        let composer = LiveComposer {
            input: std::mem::take(&mut self.input),
            input_cursor: std::mem::take(&mut self.input_cursor),
            input_images: std::mem::take(&mut self.input_images),
            input_files: std::mem::take(&mut self.input_files),
            next_image_number: std::mem::take(&mut self.next_image_number),
            next_file_number: std::mem::take(&mut self.next_file_number),
            submitting_images: std::mem::take(&mut self.submitting_images),
            submitting_files: std::mem::take(&mut self.submitting_files),
            pending_attachment_markers: std::mem::take(&mut self.pending_attachment_markers),
            input_generation: std::mem::take(&mut self.input_generation),
            prompt_history: std::mem::take(&mut self.prompt_history),
            history_index: std::mem::take(&mut self.history_index),
            history_draft: std::mem::take(&mut self.history_draft),
            history_draft_images: std::mem::take(&mut self.history_draft_images),
            history_draft_files: std::mem::take(&mut self.history_draft_files),
            kill_buffer: std::mem::take(&mut self.kill_buffer),
            kill_images: std::mem::take(&mut self.kill_images),
            kill_files: std::mem::take(&mut self.kill_files),
            chain_kill: std::mem::take(&mut self.chain_kill),
            preferred_column: std::mem::take(&mut self.preferred_column),
            queued_prompts: std::mem::take(&mut self.queued_prompts),
            pending_queue_removals: std::mem::take(&mut self.pending_queue_removals),
            pending_queue_images: std::mem::take(&mut self.pending_queue_images),
            pending_queue_files: std::mem::take(&mut self.pending_queue_files),
            history_search: std::mem::take(&mut self.history_search),
            next_history_search_generation: std::mem::take(
                &mut self.next_history_search_generation,
            ),
            composer_requests: std::mem::take(&mut self.composer_requests),
            session_history: std::mem::take(&mut self.session_history),
            session_history_error: std::mem::take(&mut self.session_history_error),
            session_history_generation: self.session_history_generation,
            pending_submissions: std::mem::take(&mut self.pending_submissions),
            submission_renders: std::mem::take(&mut self.submission_renders),
            plan_command_pending: std::mem::take(&mut self.plan_command_pending),
            operation_feedback: std::mem::take(&mut self.operation_feedback),
            deferred_form_results: std::mem::take(&mut self.deferred_form_results),
            conversation_notices: std::mem::take(&mut self.conversation_notices),
            composer_runtime: std::mem::take(&mut self.composer_runtime),
        };
        self.sync_inline_submissions();
        Some(composer)
    }
    pub fn adopt_live_composer(&mut self, composer: LiveComposer) {
        self.composer_forward = None;
        self.input = composer.input;
        self.input_cursor = composer.input_cursor;
        self.input_images = composer.input_images;
        self.input_files = composer.input_files;
        self.next_image_number = composer.next_image_number;
        self.next_file_number = composer.next_file_number;
        self.submitting_images = composer.submitting_images;
        self.submitting_files = composer.submitting_files;
        self.pending_attachment_markers = composer.pending_attachment_markers;
        self.input_generation = composer.input_generation;
        self.prompt_history = composer.prompt_history;
        self.history_index = composer.history_index;
        self.history_draft = composer.history_draft;
        self.history_draft_images = composer.history_draft_images;
        self.history_draft_files = composer.history_draft_files;
        self.kill_buffer = composer.kill_buffer;
        self.kill_images = composer.kill_images;
        self.kill_files = composer.kill_files;
        self.chain_kill = composer.chain_kill;
        self.preferred_column = composer.preferred_column;
        self.queued_prompts = composer.queued_prompts;
        self.pending_queue_removals = composer.pending_queue_removals;
        self.pending_queue_images = composer.pending_queue_images;
        self.pending_queue_files = composer.pending_queue_files;
        self.history_search = composer.history_search;
        self.next_history_search_generation = composer.next_history_search_generation;
        self.composer_requests = composer.composer_requests;
        self.session_history = composer.session_history;
        self.session_history_error = composer.session_history_error;
        self.session_history_generation = composer.session_history_generation;
        self.pending_submissions = composer.pending_submissions;
        self.submission_renders = composer.submission_renders;
        self.plan_command_pending = composer.plan_command_pending;
        self.operation_feedback = composer.operation_feedback;
        self.deferred_form_results = composer.deferred_form_results;
        self.conversation_notices = composer.conversation_notices;
        self.composer_runtime = composer.composer_runtime;
        self.sync_inline_submissions();
        self.update_autocomplete();
    }
    /// `Some(None)` means Move preparation has not supplied its stable identity yet.
    pub fn set_composer_move_origin(&mut self, origin: Option<(u64, Option<String>)>) {
        self.composer_runtime.origin = match origin {
            None => PromptOrigin::Ordinary,
            Some((generation, None)) => PromptOrigin::PreparingMove(generation),
            Some((generation, Some(id))) => {
                if let Some(deferred) = self.composer_runtime.submit_when_ready.as_mut()
                    && matches!(deferred.origin,PromptOrigin::PreparingMove(expected) if expected==generation)
                {
                    deferred.origin = PromptOrigin::Move(id.clone());
                }
                PromptOrigin::Move(id)
            }
        };
    }
    pub fn settle_deferred_move_origin(
        &mut self,
        generation: u64,
        id: Option<&str>,
        succeeded: bool,
    ) {
        let Some(deferred) = self.composer_runtime.submit_when_ready.as_mut() else {
            return;
        };
        let matches = match &deferred.origin {
            PromptOrigin::PreparingMove(expected) => *expected == generation,
            PromptOrigin::Move(expected) => Some(expected.as_str()) == id,
            _ => false,
        };
        if !matches {
            return;
        }
        if succeeded {
            if let Some(id) = id {
                deferred.origin = PromptOrigin::Move(id.to_owned());
            }
        } else {
            self.keep_deferred_submission_as_draft();
        }
    }
    pub(super) fn defer_startup_submission(&mut self) {
        self.composer_runtime.submit_when_ready = Some(DeferredSubmission {
            origin: self.composer_runtime.origin.clone(),
        });
    }
    /// A launch has no attachment store until registration supplies the real ID.
    pub fn bind_composer_session(&mut self, session_id: Option<&str>) {
        self.composer_runtime.session_id = session_id.map(str::to_owned);
        self.pump_attachment_queue();
    }
    pub fn handle_composer_paste(&mut self, pasted: &str) {
        let action = self.handle_terminal_paste(pasted);
        self.dispatch_composer_action(action);
    }
    pub fn dispatch_composer_action(&mut self, action: ChatAction) {
        match action {
            ChatAction::PasteFromClipboard => self.read_clipboard(),
            ChatAction::Attach { path, command } => {
                let is_image = path.is_image_extension();
                let source = self.attachment_source(path, is_image);
                self.queue_attachment(source, AttachmentFallback::Command(command));
            }
            ChatAction::AttachPaste { text, paths } => {
                let mut cursor = 0;
                let mut rejection = None;
                for path in paths {
                    self.paste_plain(&text[cursor..path.range.start]);
                    let pasted = &text[path.range.clone()];
                    let source = self.attachment_source(path.path, path.is_image);
                    if !self.queue_attachment(source, AttachmentFallback::Text(pasted.to_owned())) {
                        rejection = self.notice();
                        self.paste_plain(pasted);
                    }
                    cursor = path.range.end;
                }
                self.paste_plain(&text[cursor..]);
                if let Some(notice) = rejection {
                    self.set_notice(notice);
                }
            }
            _ => {}
        }
    }
    pub(super) fn path_uses_image_mode(&self, path: &pasted_paths::PastedPath) -> bool {
        self.uses_image_mode(path.is_image_extension())
    }

    fn uses_image_mode(&self, is_image: bool) -> bool {
        is_image
            && (self.standby
                || self.composer_runtime.accepting_staged
                || self.prompt_images_supported)
    }

    fn attachment_source(
        &self,
        path: pasted_paths::PastedPath,
        is_image: bool,
    ) -> AttachmentSource {
        if self.uses_image_mode(is_image) {
            AttachmentSource::ImagePath(path)
        } else {
            AttachmentSource::FilePath(path)
        }
    }

    fn read_clipboard(&mut self) {
        if self.composer_runtime.paste_in_flight {
            self.set_notice("Clipboard read already in progress…");
            return;
        }
        self.composer_runtime.paste_in_flight = true;
        self.set_notice("Reading clipboard…");
        let updates = self.composer_runtime.tx.clone();
        let text_only = self.clipboard_is_text_only();
        let generation = self.input_generation();
        let target = self.clipboard_target();
        let staging = self.standby;
        mj_core::runtime::spawn(async move {
            let result = match mj_core::runtime::spawn_blocking(move || {
                if text_only {
                    crate::clipboard::read_text().map(ClipboardContent::Text)
                } else {
                    crate::clipboard::read()
                }
                .map_err(|error| format!("{error:#}"))
            })
            .await
            {
                Ok(result) => result,
                Err(error) => Err(format!("clipboard task failed: {error}")),
            };
            if let Err(error) = updates.send(ComposerIoUpdate::Clipboard {
                generation,
                target,
                staging,
                result,
            }) {
                tracing::debug!(%error, "clipboard result dropped because the composer closed");
            }
        });
    }
    /// Returning to a retained worker must not send a prompt intended for its replacement.
    pub fn keep_deferred_submission_as_draft(&mut self) {
        if self.composer_runtime.submit_when_ready.take().is_some() {
            self.set_notice("The operation ended before the prompt was ready; the draft is kept.");
        }
    }
    pub fn take_prepared_startup_prompts(&mut self) -> Vec<StartupPrompt> {
        self.composer_runtime.prepared_startup.drain(..).collect()
    }
    pub fn has_composer_updates(&self) -> bool {
        !self.composer_runtime.rx.is_empty()
    }
    pub fn drain_composer_updates(&mut self) {
        while let Ok(update) = self.composer_runtime.rx.try_recv() {
            self.apply_composer_update(update);
        }
    }
    pub fn drain_composer(&mut self) -> Option<StartupPrompt> {
        self.drain_composer_updates();
        self.pump_attachment_queue();
        self.composer_runtime
            .remote_streams
            .retain(|stream| !stream.is_finished());
        let deferred = self.composer_runtime.submit_when_ready.as_ref()?;
        if self.composer_runtime.paste_in_flight || self.has_pending_attachments() {
            return None;
        }
        let origin = match &deferred.origin {
            PromptOrigin::PreparingMove(_) => return None,
            origin => origin.clone(),
        };
        self.composer_runtime.submit_when_ready = None;
        let current_origin = std::mem::replace(&mut self.composer_runtime.origin, origin);
        let was_standby = std::mem::replace(&mut self.standby, true);
        let prompt = match self.submit_input() {
            ChatAction::Prompt(_) => self.latest_startup_prompt(),
            action => {
                self.dispatch_composer_action(action);
                None
            }
        };
        self.standby = was_standby;
        self.composer_runtime.origin = current_origin;
        prompt
    }
    pub(super) fn apply_composer_update(&mut self, update: ComposerIoUpdate) {
        match update {
            ComposerIoUpdate::SessionHistoryListed { generation, result } => match result {
                Ok(entries) => self.set_session_history(generation, entries),
                Err(error) => {
                    tracing::warn!(%error, "session chat history listing failed");
                    self.set_session_history_unavailable(generation, error);
                }
            },
            ComposerIoUpdate::HistorySearchResults { generation, result } => {
                self.apply_history_search_results(generation, result)
            }
            ComposerIoUpdate::Remote(result) => remote::apply_chat_remote_result(self, result),
            ComposerIoUpdate::Clipboard {
                generation,
                target,
                result,
                staging,
            } => {
                self.composer_runtime.paste_in_flight = false;
                // An accepted form paste belongs to its request, even when
                // the other presentation (or its history reader) redraws.
                let target_matches = match &target {
                    input_state::ClipboardTarget::Answer(_) => {
                        target == self.elicitation_clipboard_target()
                    }
                    _ => target == self.clipboard_target(),
                };
                if generation != self.input_generation() || !target_matches {
                    return;
                }
                self.composer_runtime.accepting_staged = staging;
                let submit_when_ready = if result.is_ok() {
                    self.composer_runtime.submit_when_ready.clone()
                } else {
                    None
                };
                match result {
                    Ok(ClipboardContent::Image(image)) => {
                        self.queue_attachment(
                            AttachmentSource::Clipboard(image),
                            AttachmentFallback::FailedMarker,
                        );
                    }
                    // Copied files arrive as their paths, which attach the
                    // way a pasted path does.
                    Ok(ClipboardContent::Text(text))
                        if target == input_state::ClipboardTarget::Composer =>
                    {
                        let action = self.paste_or_attach(&text);
                        self.dispatch_composer_action(action);
                    }
                    Ok(content) => self.handle_clipboard_content(content),
                    Err(error) => {
                        tracing::warn!(%error, "clipboard read failed and was shown in the UI");
                        let policy = if self.standby || self.prompt_images_supported {
                            ""
                        } else {
                            "; this agent accepts only clipboard text"
                        };
                        self.set_notice(format!("Paste failed: {error}{policy}"));
                    }
                }
                self.composer_runtime.accepting_staged = false;
                self.composer_runtime.submit_when_ready = submit_when_ready;
            }
            ComposerIoUpdate::ReceiptPrepared { command_id, result } => {
                self.finish_receipt_preparation(&command_id, result);
            }
            ComposerIoUpdate::AttachmentFinished(result) => {
                self.apply_attachment_result(result);
                self.pump_attachment_queue();
            }
        }
    }
    /// Reserve a marker at the cursor and read the attachment behind it.
    /// `false` when no marker could be reserved; the notice says why.
    pub(super) fn queue_attachment(
        &mut self,
        source: AttachmentSource,
        fallback: AttachmentFallback,
    ) -> bool {
        let sequence = self.composer_runtime.next_attachment_sequence;
        let reserved = match &source {
            AttachmentSource::FilePath(path) => self.reserve_file_attachment(sequence, path),
            _ => self.reserve_attachment(sequence),
        };
        if !reserved {
            return false;
        }
        self.composer_runtime.next_attachment_sequence = self
            .composer_runtime
            .next_attachment_sequence
            .wrapping_add(1);
        self.composer_runtime
            .attachment_queue
            .push_back((sequence, source, fallback));
        self.pump_attachment_queue();
        true
    }

    pub(crate) fn pump_attachment_queue(&mut self) {
        // Older drafts and conversation history may carry inline bytes. Adopt
        // them into the same marker-owned jobs before they can be submitted.
        for image in &mut self.input_images {
            if image.image.reference.is_none() && !image.image.is_placeholder() {
                let sequence = self.composer_runtime.next_attachment_sequence;
                self.composer_runtime.next_attachment_sequence = sequence.wrapping_add(1);
                let source = std::mem::replace(&mut image.image, ClipboardImage::pending());
                self.pending_attachment_markers
                    .insert(sequence, PendingMarker::Image(image.number));
                self.composer_runtime.attachment_queue.push_back((
                    sequence,
                    AttachmentSource::Clipboard(source),
                    AttachmentFallback::FailedMarker,
                ));
                self.input_generation = self.input_generation.wrapping_add(1);
            }
        }
        self.pending_attachment_markers
            .retain(|_, marker| match marker {
                PendingMarker::Image(number) => self
                    .input_images
                    .iter()
                    .any(|image| image.number == *number && image.image.is_pending()),
                PendingMarker::File(number) => self
                    .input_files
                    .iter()
                    .any(|file| file.number == *number && file.file.is_pending()),
            });
        self.composer_runtime
            .attachment_cancellations
            .retain(|sequence, _| self.pending_attachment_markers.contains_key(sequence));
        self.composer_runtime
            .attachment_queue
            .retain(|(sequence, _, _)| self.pending_attachment_markers.contains_key(sequence));
        while self.composer_runtime.attachment_tasks_in_flight < MAX_ATTACHMENT_TASKS {
            let Some(session_id) = self.composer_runtime.session_id.clone() else {
                break;
            };
            let Some((sequence, source, fallback)) =
                self.composer_runtime.attachment_queue.pop_front()
            else {
                break;
            };
            self.composer_runtime.attachment_tasks_in_flight += 1;
            let updates = self.composer_runtime.tx.clone();
            let (cancel, cancelled) = tokio::sync::oneshot::channel();
            self.composer_runtime
                .attachment_cancellations
                .insert(sequence, cancel);
            mj_core::runtime::spawn(async move {
                let upload_session = session_id;
                let preparation =
                    async move { prepare_composer_attachment(&upload_session, source).await };
                let result = tokio::select! {
                    result = preparation => result,
                    _ = cancelled => Err(AttachmentFailure::error("attachment cancelled")),
                };
                if let Err(error) =
                    updates.send(ComposerIoUpdate::AttachmentFinished(AttachmentResult {
                        sequence,
                        fallback,
                        result,
                    }))
                {
                    tracing::debug!(%error, "attachment result dropped because the composer closed");
                }
            });
        }
    }

    pub(super) fn apply_attachment_result(&mut self, result: AttachmentResult) {
        self.composer_runtime.attachment_tasks_in_flight = self
            .composer_runtime
            .attachment_tasks_in_flight
            .saturating_sub(1);
        self.composer_runtime
            .attachment_cancellations
            .remove(&result.sequence);
        self.finish_attachment(result.sequence, result.result, result.fallback);
    }
}

/// Prepare and install the attachment off the composer event loop.
pub(super) async fn prepare_composer_attachment(
    session_id: &str,
    source: AttachmentSource,
) -> Result<ReadyAttachment, AttachmentFailure> {
    let path = match source {
        AttachmentSource::Clipboard(image) => {
            return prepare_attachment(session_id, image)
                .await
                .map(ReadyAttachment::Image)
                .map_err(AttachmentFailure::Error);
        }
        AttachmentSource::ImagePath(path) => {
            let session = session_id.to_owned();
            let image = mj_core::runtime::spawn_blocking(move || {
                let path = path.resolve().map_err(AttachmentFailure::error)?;
                attachments::install_path(&session, &path)
            })
            .await
            .map_err(AttachmentFailure::error)??;
            return attachments::upload_prepared_image(session_id, image)
                .await
                .map(ReadyAttachment::Image)
                .map_err(|error| AttachmentFailure::Error(format!("{error:#}")));
        }
        AttachmentSource::FilePath(path) => path,
    };
    let upload = mj_core::runtime::spawn_blocking(move || {
        let path = path.resolve().map_err(AttachmentFailure::error)?;
        attachments::read_file(&path)
    })
    .await
    .map_err(AttachmentFailure::error)??;
    let reference = mj_client::daemon::connect_existing()
        .await
        .map_err(AttachmentFailure::error)?
        .http()
        .await
        .map_err(AttachmentFailure::error)?
        .upload_file_attachment(session_id, &upload.filename, upload.bytes, upload.mime_type)
        .await
        .map_err(|error| AttachmentFailure::Error(format!("{error:#}")))?;
    AttachedFile::from_reference(reference)
        .map(ReadyAttachment::File)
        .map_err(AttachmentFailure::error)
}

/// Shared clipboard codec and daemon-install path for composer markers and old receipts.
pub(super) async fn prepare_attachment(
    session_id: &str,
    image: ClipboardImage,
) -> Result<ClipboardImage, String> {
    let blocking_session = session_id.to_owned();
    let image = mj_core::runtime::spawn_blocking(move || {
        attachments::install_clipboard_image(&blocking_session, image)
    })
    .await
    .map_err(|error| format!("attachment task failed: {error}"))?
    .map_err(|error| format!("{error:#}"))?;
    attachments::upload_prepared_image(session_id, image)
        .await
        .map_err(|error| format!("{error:#}"))
}
