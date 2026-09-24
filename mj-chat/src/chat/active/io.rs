use super::*;

impl ActiveChat {
    /// Waits for the next background message, applies it, and drains whatever
    /// queued behind it.
    ///
    /// `None` means no chat is warm, and the feed never wakes the caller. Cancel
    /// safe: every arm is a cancel-safe receive, and a message is applied only
    /// once its arm has won.
    pub async fn pump(chat: Option<&mut Self>) {
        let Some(chat) = chat else {
            return std::future::pending().await;
        };
        enum Wakeup {
            Remote(Option<ChatRemoteResult>),
            Io(ChatIoUpdate),
            Voice(VoiceUpdate),
            // Boxed: a view carries the whole session snapshot, and the enum
            // is built on every wakeup.
            View(Box<Result<ManagedSessionView>>),
        }
        // The senders for the I/O and voice feeds live in this struct, so those
        // receivers cannot report a closed channel and need no retirement flag.
        let wakeup = tokio::select! {
            result = chat.remote.recv(), if chat.remote_open => Wakeup::Remote(result),
            Some(update) = chat.chat_io_rx.recv() => Wakeup::Io(update),
            Some(update) = chat.voice_updates_rx.recv() => Wakeup::Voice(update),
            view = chat.session.changed(), if chat.session_open => Wakeup::View(Box::new(view)),
        };
        match wakeup {
            Wakeup::Remote(Some(result)) => chat.apply_remote_result(result),
            Wakeup::Remote(None) => chat.remote_open = false,
            Wakeup::Io(update) => chat.apply_io_update(update),
            Wakeup::Voice(update) => chat.apply_voice_update(update),
            Wakeup::View(view) => chat.apply_session_view(*view),
        }
        chat.drain().await;
        chat.report_worker_death().await;
    }

    pub(crate) async fn drain(&mut self) {
        self.refresh_voice_availability();
        while let Ok(result) = self.remote.try_recv() {
            self.apply_remote_result(result);
        }
        while let Ok(update) = self.chat_io_rx.try_recv() {
            self.apply_io_update(update);
        }
        while let Ok(update) = self.voice_updates_rx.try_recv() {
            self.apply_voice_update(update);
        }
        while self.session_open && self.session.has_changed().unwrap_or(false) {
            let view = self.session.changed().await;
            self.apply_session_view(view);
        }
        self.advance_review();
        self.publish_conversation_notices();
    }

    pub(super) fn publish_conversation_notices(&mut self) {
        for notice in &mut self.state.conversation_notices {
            if notice.submitted {
                continue;
            }
            match self
                .remote
                .operations()
                .try_send(ChatRemoteOperation::RecordNotice {
                    id: notice.id.clone(),
                    text: notice.text.clone(),
                }) {
                Ok(()) => notice.submitted = true,
                Err(error) => tracing::warn!(%error, "could not queue conversation notice"),
            }
        }
    }

    /// Moves a review on when the planner has answered the context request.
    ///
    /// The answer is the planner's next agent message after the request went
    /// out, so a message already in the transcript can never be mistaken for
    /// it, and a reconnect that replays the same completion starts no second
    /// reviewer turn.
    pub(crate) fn advance_review(&mut self) {
        let Some(SecondOpinion::Review(review)) = self.state.second_opinion() else {
            return;
        };
        let context_baseline = review.context_baseline;
        let mj_core::second_opinion::ReviewStage::GatheringContext { command_id } =
            review.workflow.stage()
        else {
            return;
        };
        if self.state.phase != WorkerPhase::Idle {
            return;
        }
        let command_id = command_id.clone();
        let Some(summary) = self.state.latest_agent_text_after(context_baseline) else {
            return;
        };
        let reviewer_command_id = self.state.next_second_opinion_command_id("review");
        let Some(SecondOpinion::Review(review)) = self.state.second_opinion_mut() else {
            return;
        };
        let Some(request) =
            review
                .workflow
                .primary_context_completed(&command_id, summary, reviewer_command_id)
        else {
            return;
        };
        if let Some(view) = self.state.second_opinion_mut() {
            view.set_status("the reviewer is reading the plan…");
        }
        self.persist_review();
        self.run_workflow_request(request);
    }

    /// Shows whatever review the daemon is running for this session.
    ///
    /// The terminal hosts no part of a review: it renders this view, reads the
    /// reviewing roles' journals to show their transcripts, and sends
    /// resolutions back. A review therefore survives this view closing.
    pub fn apply_review_view(&mut self, view: Option<mj_client::review::RuntimeReviewView>) {
        let roles = view
            .as_ref()
            .map(|view| {
                view.roles
                    .iter()
                    .map(|role| role.role.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.state.set_turn_review(view);
        if self.state.turn_review().is_none() {
            self.reviewed_roles.clear();
            // The daemon's review projection is also the authority for
            // reviewer elicitations. Reconcile here so a form disappears as
            // soon as an external answer closes the review, even when no
            // reviewer journal event arrives afterward.
            self.surface_reviewer_elicitations();
            return;
        }
        // One reader per role, started the first time the daemon names it.
        for role in roles {
            if self.reviewed_roles.insert(role.clone()) {
                self.poll_turn_review_role(&role);
            }
        }
        self.surface_reviewer_elicitations();
    }

    /// Asks the daemon to review the turn that just finished.
    pub(crate) fn request_turn_review(&mut self) {
        let session_id = self.session.session_id().to_owned();
        self.state.set_notice("Starting a review…");
        self.send_daemon_request(ChatDaemonRequest::StartTurnReview { session_id });
    }

    /// Sends one resolution to the daemon, which owns the review.
    pub(crate) fn run_turn_review(&mut self, intent: crate::chat::TurnReviewRequest) {
        use crate::chat::TurnReviewRequest;

        let session_id = self.session.session_id().to_owned();
        match intent {
            TurnReviewRequest::Start => self.request_turn_review(),
            TurnReviewRequest::Resolve(resolution) => {
                self.send_daemon_request(ChatDaemonRequest::ResolveTurnReview {
                    session_id,
                    resolution,
                });
            }
        }
    }

    /// Hands one request to the daemon bridge, or reports that this chat has
    /// none: a chat without a bridge cannot reach the daemon at all, and
    /// silently dropping a review action would leave the pane sitting there.
    pub(crate) fn send_daemon_request(&mut self, request: ChatDaemonRequest) {
        let Some(persistence) = &self.persistence else {
            self.state
                .set_notice("This chat cannot reach the Mjolnir daemon");
            return;
        };
        if let Err(error) = persistence.send(request) {
            tracing::warn!(%error, "a review request could not be queued for the daemon");
            self.state.set_notice("The Mjolnir daemon is not reachable");
        }
    }

    /// Reports a daemon refusal in the review pane, or as a notice when no
    /// review is open -- a refused `/review` has nowhere else to appear.
    pub fn report_review_refusal(&mut self, message: String) {
        match self.state.turn_review_mut() {
            Some(review) => {
                review.report_failure(message);
            }
            None => self.state.set_notice(message),
        }
    }

    /// Reports a background worker that stopped on its own. Cheap enough to
    /// check on every wakeup: it only joins a handle that already finished.
    pub(crate) async fn report_worker_death(&mut self) {
        let Some(result) = self.remote.take_finished().await else {
            return;
        };
        apply_chat_remote_result(
            &mut self.state,
            ChatRemoteResult::WorkerFailed(match result {
                Err(error) => format!("Chat background worker failed: {error}"),
                Ok(()) => "Chat background worker stopped unexpectedly".into(),
            }),
        );
    }

    pub(super) fn apply_io_update(&mut self, update: ChatIoUpdate) {
        let update = match update {
            ChatIoUpdate::SessionReconnected(result) => {
                self.finish_session_reconnect(result);
                return;
            }
            ChatIoUpdate::ReviewerPrepared {
                generation,
                result,
                acknowledged,
            } => {
                self.apply_reviewer_prepared(generation, result);
                let _ = acknowledged.send(());
                return;
            }
            ChatIoUpdate::ReviewerNotice(notice) => {
                self.state.conversation_notice(notice);
                return;
            }
            ChatIoUpdate::ReviewerStarted(result) => {
                if let Err(error) = result {
                    if let Some(view) = self.state.second_opinion_mut() {
                        view.report_failure(error);
                    } else {
                        self.report_review_refusal(error);
                    }
                }
                return;
            }
            ChatIoUpdate::ReviewerEvents { result } => {
                self.apply_reviewer_events(result);
                return;
            }
            ChatIoUpdate::TurnReviewEvents { role, result } => {
                self.apply_turn_review_role_events(role, result);
                return;
            }
            ChatIoUpdate::Clipboard {
                generation,
                target,
                result,
            } => {
                self.paste_in_flight = false;
                if generation != self.state.input_generation()
                    || target != self.state.clipboard_target()
                {
                    return;
                }
                match result {
                    Ok(ClipboardContent::Image(image)) => {
                        self.queue_attachment(AttachmentSource::Clipboard(image), None);
                    }
                    Ok(content) => self.state.handle_clipboard_content(content),
                    Err(error) => {
                        tracing::warn!(%error, "clipboard read failed and was shown in the UI");
                        let policy = if self.state.prompt_images_supported {
                            ""
                        } else {
                            "; this agent accepts only clipboard text"
                        };
                        self.state
                            .set_notice(format!("Paste failed: {error}{policy}"));
                    }
                }
                return;
            }
            ChatIoUpdate::AttachmentFinished(result) => {
                self.apply_attachment_result(result);
                self.pump_attachment_queue();
                return;
            }
            update => update,
        };
        if matches!(&update, ChatIoUpdate::ToolDiffstats { .. }) {
            self.diffstats_in_flight = self.diffstats_in_flight.saturating_sub(1);
        }
        if let PrefixRebuild::Needed { attempt } = apply_chat_io_update(&mut self.state, update) {
            self.rebuild_transcript_prefix(attempt);
        }
        dispatch_history_search_request(self.session.clone(), &mut self.state, &self.chat_io_tx);
        dispatch_diffstat_requests(
            &mut self.state,
            &self.chat_io_tx,
            &mut self.diffstats_in_flight,
        );
    }

    pub(super) fn queue_attachment(&mut self, source: AttachmentSource, command: Option<String>) {
        let sequence = self.next_attachment_sequence;
        if !self.state.reserve_attachment(sequence) {
            return;
        }
        self.next_attachment_sequence = self.next_attachment_sequence.wrapping_add(1);
        self.attachment_queue.push_back((sequence, source, command));
        self.pump_attachment_queue();
    }

    pub(crate) fn pump_attachment_queue(&mut self) {
        while self.attachment_tasks_in_flight < MAX_ATTACHMENT_TASKS {
            let Some((sequence, source, command)) = self.attachment_queue.pop_front() else {
                break;
            };
            self.attachment_tasks_in_flight += 1;
            let session_id = self.session.session_id().to_owned();
            let updates = self.chat_io_tx.clone();
            tokio::spawn(async move {
                let result = match tokio::task::spawn_blocking(move || match source {
                    AttachmentSource::Clipboard(image) => {
                        attachments::install_clipboard_image(&session_id, image)
                    }
                    AttachmentSource::Path(path) => attachments::install_path(&session_id, &path),
                })
                .await
                {
                    Ok(result) => result.map_err(|error| format!("{error:#}")),
                    Err(error) => Err(format!("attachment task failed: {error}")),
                };
                if let Err(error) =
                    updates.send(ChatIoUpdate::AttachmentFinished(AttachmentResult {
                        sequence,
                        command,
                        result,
                    }))
                {
                    tracing::debug!(%error, "attachment result dropped because the chat closed");
                }
            });
        }
    }

    pub(super) fn apply_attachment_result(&mut self, result: AttachmentResult) {
        self.attachment_tasks_in_flight = self.attachment_tasks_in_flight.saturating_sub(1);
        self.state
            .finish_attachment(result.sequence, result.result, result.command);
    }

    /// Restarts the history conversion against the session's current snapshot,
    /// after the transcript changed under the last one. Only the spawn happens
    /// here; the conversion itself stays off the event loop.
    pub(crate) fn rebuild_transcript_prefix(&mut self, attempt: u32) {
        let view = self.session.view();
        let Some(snapshot) = view.snapshot else {
            return;
        };
        let Some(pending) =
            PendingPrefix::of(&snapshot.materialized, self.state.unconverted_prefix())
        else {
            return;
        };
        spawn_transcript_prefix(pending, attempt, self.chat_io_tx.clone());
    }

    /// The sentence the dictation key shows when dictation cannot start, so
    /// the key never does nothing without saying why (launch findings A-4
    /// and E-9).
    pub(crate) fn dictation_unavailable_notice(&self) -> String {
        self.voice_unavailable
            .clone()
            .unwrap_or_else(|| DICTATION_NOT_CHECKED.to_owned())
    }

    pub(crate) fn refresh_voice_availability(&mut self) {
        let Some(context) = &self.context else {
            return;
        };
        let paths = mj_client::auth::auth_paths(&context.config, &context.session.last_profile);
        if paths != self.voice_probe_paths {
            self.voice_probe_paths.clone_from(&paths);
            self.voice_auth = None;
            self.voice_unavailable = None;
            self.state.set_voice_available(false);
            self.voice_probe_at = None;
        }
        if self.voice_probe_pending
            || self
                .voice_probe_at
                .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return;
        }
        self.voice_probe_pending = true;
        self.voice_probe_at = Some(std::time::Instant::now());
        let updates = self.voice_updates_tx.clone();
        tokio::spawn(async move {
            let probed_paths = paths.clone();
            let result = tokio::task::spawn_blocking(move || {
                if let Some(reason) = crate::speech::voice_input_unavailable_reason() {
                    return Err(reason);
                }
                if paths.is_empty() {
                    return Err(DICTATION_NEEDS_A_CODEX_PROFILE.to_owned());
                }
                mj_client::auth::available_auth(paths)
                    .ok_or_else(|| DICTATION_NEEDS_A_CHATGPT_SIGN_IN.to_owned())
            })
            .await;
            let result = result
                .map_err(|error| anyhow::anyhow!("dictation availability task failed: {error}"));
            if let Err(error) = updates.send(VoiceUpdate::Availability(probed_paths, result)) {
                tracing::debug!(%error, "dictation availability dropped because the chat closed");
            }
        });
    }

    pub(super) fn apply_voice_update(&mut self, update: VoiceUpdate) {
        match update {
            VoiceUpdate::Availability(paths, result) => {
                self.voice_probe_pending = false;
                if paths != self.voice_probe_paths {
                    return;
                }
                match result {
                    Ok(Ok(path)) => {
                        self.state.set_voice_available(true);
                        self.voice_auth = Some(path);
                        self.voice_unavailable = None;
                    }
                    Ok(Err(reason)) => {
                        self.state.set_voice_available(false);
                        self.voice_auth = None;
                        self.voice_unavailable = Some(reason);
                    }
                    Err(error) => self.state.set_notice(error.to_string()),
                }
            }
            VoiceUpdate::Status(status) => self.state.set_notice(status),
            VoiceUpdate::Finished(result) => {
                self.state.voice_active = false;
                self.voice_cancel = None;
                self.voice_finishing = false;
                match result {
                    Ok(text) => {
                        if !text.trim().is_empty() {
                            self.state
                                .set_input(append_dictation(&self.state.input, &text));
                        }
                        self.state.clear_notice();
                    }
                    Err(error) => self
                        .state
                        .set_notice(crate::speech::dictation_error_message(&error)),
                }
            }
        }
    }

    pub(crate) fn apply_session_view(&mut self, view: Result<ManagedSessionView>) {
        if self.session_retiring
            && let Err(error) = &view
        {
            // The feed closing is the expected end of a deliberate stop or
            // destroy, so it is neither a lost connection nor a reason to
            // chase a replacement actor. The transcript stays readable.
            tracing::debug!(
                error = format!("{error:#}"),
                session_id = %self.session.session_id(),
                "session feed closed because the session is being retired"
            );
            self.session_open = false;
            return;
        }
        self.session_open = apply_session_view(&mut self.state, view);
        self.apply_deferred_elicitation_draft();
        self.surface_reviewer_elicitations();
        if !self.session_open && !self.session_retiring {
            self.begin_session_reconnect();
        }
        dispatch_diffstat_requests(
            &mut self.state,
            &self.chat_io_tx,
            &mut self.diffstats_in_flight,
        );
    }

    pub(super) fn apply_remote_result(&mut self, result: ChatRemoteResult) {
        let sync_succeeded = matches!(&result, ChatRemoteResult::Sync(Ok(())));
        let sync_finished = matches!(&result, ChatRemoteResult::Sync(_));
        apply_chat_remote_result(&mut self.state, result);
        if sync_finished {
            if sync_succeeded && self.reconnect_notice_pending_sync {
                self.state.connection_feedback = None;
            }
            self.reconnect_notice_pending_sync = false;
        }
    }

    pub(crate) fn begin_session_reconnect(&mut self) {
        if self.session_reconnect_in_flight {
            return;
        }
        self.session_reconnect_in_flight = true;
        let session_id = self.session.session_id().to_owned();
        let session_manager = self.session_manager.clone();
        let updates = self.chat_io_tx.clone();
        tokio::spawn(async move {
            let result = session_manager
                .wait_for_session(&session_id, SESSION_ACTOR_RECONNECT_WAIT)
                .await
                .map_err(|error| format!("{error:#}"));
            if let Err(error) = updates.send(ChatIoUpdate::SessionReconnected(result)) {
                tracing::debug!(
                    %error,
                    %session_id,
                    "session reconnect result dropped because the chat closed"
                );
            }
        });
    }

    pub(crate) fn finish_session_reconnect(
        &mut self,
        result: std::result::Result<ManagedSessionHandle, String>,
    ) {
        self.session_reconnect_in_flight = false;
        match result {
            Ok(session) => {
                let view = session.view();
                self.session = session;
                self.session_open = apply_session_view(&mut self.state, Ok(view));
                self.apply_deferred_elicitation_draft();
                if self.session_open {
                    self.reconnect_notice_pending_sync = true;
                    self.state.connection_feedback = None;
                } else {
                    self.begin_session_reconnect();
                }
            }
            Err(error) => {
                if self.session_retiring {
                    // The session was stopped or destroyed on purpose, so
                    // losing its actor is the expected outcome, not a failure.
                    tracing::debug!(
                        %error,
                        session_id = %self.session.session_id(),
                        "session relay handoff ended because the session is being retired"
                    );
                    return;
                }
                self.state.set_connection_notice(format!(
                    "Could not reconnect to session relay: {error}"
                ));
                if self.session_feed_expected {
                    self.begin_session_reconnect();
                }
            }
        }
    }
}

/// Shown until the first availability probe answers. A conversation opened
/// without its session's settings never probes, so this also covers that.
const DICTATION_NOT_CHECKED: &str = "Dictation is unavailable until Mjolnir has checked for the voice helper and a Codex sign-in. Try again in a moment.";
pub(super) const DICTATION_NEEDS_A_CODEX_PROFILE: &str = "Dictation is unavailable. It transcribes through a Codex profile signed in with a ChatGPT account, and no Codex profile is configured. Add one in Setup.";
const DICTATION_NEEDS_A_CHATGPT_SIGN_IN: &str = "Dictation is unavailable. It transcribes through a Codex profile signed in with a ChatGPT account, and no Codex profile is signed in that way; an API key does not work. Sign a Codex profile in with ChatGPT.";
