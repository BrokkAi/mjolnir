//! Local submission rows bridge input handling and the durable projection.
use super::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingSubmission {
    id: String,
    kind: SubmissionKind,
    payload: PromptPayload,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    startup_origin: Option<StartupRetryOrigin>,
    represented: bool,
    finished: bool,
    recorded_at_ms: i64,
    #[serde(default)]
    submitted_after: Option<u64>,
    #[serde(skip)]
    started_at: Option<std::time::Instant>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StartupRetryOrigin {
    #[serde(default)]
    inherited_draft: Option<String>,
    move_operation_id: Option<String>,
}

/// "Delivery unconfirmed: <why>", without repeating the words when the error
/// itself starts with them.
fn unconfirmed_status(error: &str) -> String {
    let marker = mj_client::session::DeliveryUnconfirmed.to_string();
    let reason = error
        .strip_prefix(marker.as_str())
        .map(|rest| rest.trim_start_matches(':').trim_start())
        .unwrap_or(error);
    if reason.is_empty() {
        "Delivery unconfirmed".to_owned()
    } else {
        format!("Delivery unconfirmed: {reason}")
    }
}

/// Whether a transcript item is the projection's record of submission `id`:
/// the user message of a prompt, the row of a shell command, or the divider
/// of an accepted `/clear`, which has no user message of its own (R4-4).
fn represents_submission(stable_id: &str, id: &str) -> bool {
    stable_id.strip_suffix(id).is_some_and(|prefix| {
        matches!(prefix, "user:" | "shell:") || prefix == mj_core::archive::CONTEXT_BOUNDARY_PREFIX
    })
}

pub(super) fn is_inline_submission_entry(entry: &ChatEntry) -> bool {
    entry.source.0.is_none()
        && entry
            .message_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local-submission:"))
}

impl PendingSubmission {
    fn is_inline(&self) -> bool {
        self.finished && !self.represented && self.status.starts_with("Delivery unconfirmed")
    }

    fn can_resend(&self) -> bool {
        self.is_inline() && self.startup_origin.is_none()
    }

    /// A `/clear` the relay runs as a context reset, not as a prompt.
    fn is_context_clear(&self) -> bool {
        self.kind == SubmissionKind::Prompt
            && self.payload.images.is_empty()
            && self.payload.text.trim() == "/clear"
    }
}

impl ChatState {
    /// Preserve exact startup identity across bounded shutdown without making it resendable.
    pub fn retain_startup_receipt(
        &mut self,
        prompt: super::StartupPrompt,
        inherited_draft: Option<String>,
    ) {
        if self
            .pending_submissions
            .iter()
            .any(|pending| pending.id == prompt.command_id)
        {
            return;
        }
        self.pending_submissions.push(PendingSubmission {
            represented: self
                .queued_prompts
                .iter()
                .any(|queued| queued.id == prompt.command_id),
            id: prompt.command_id,
            kind: SubmissionKind::Prompt,
            payload: prompt.payload,
            status: "Delivery unconfirmed: checking queued prompt".into(),
            finished: true,
            startup_origin: Some(StartupRetryOrigin {
                inherited_draft,
                move_operation_id: prompt.move_operation_id,
            }),
            recorded_at_ms: mj_core::clock::epoch_millis(),
            submitted_after: Some(self.latest_seq),
            started_at: None,
        });
        self.sync_inline_submissions();
    }
    pub fn startup_retry_prompts(&self) -> Vec<(super::StartupPrompt, Option<String>)> {
        self.pending_submissions
            .iter()
            .filter_map(|pending| {
                pending.startup_origin.as_ref().map(|origin| {
                    (
                        super::StartupPrompt {
                            command_id: pending.id.clone(),
                            payload: pending.payload.clone(),
                            move_operation_id: origin.move_operation_id.clone(),
                        },
                        origin.inherited_draft.clone(),
                    )
                })
            })
            .collect()
    }
    pub(super) fn settle_startup_receipt(&mut self, command_id: &str) {
        self.pending_submissions
            .retain(|pending| pending.id != command_id || pending.startup_origin.is_none());
        self.sync_inline_submissions();
    }

    pub(super) fn restore_submissions(&mut self, pending: Vec<PendingSubmission>) {
        self.pending_submissions = pending;
        for pending in &mut self.pending_submissions {
            pending.finished = true;
            if !pending.status.starts_with("Delivery unconfirmed") {
                pending.status = "Delivery unconfirmed".into();
            }
            // Older saved receipts have no frontier. Choose their anchor once
            // on restore from the timestamps of the available durable entries.
            pending.submitted_after.get_or_insert_with(|| {
                self.entries
                    .iter()
                    .filter(|entry| {
                        entry
                            .recorded_at_ms
                            .is_some_and(|at| at <= pending.recorded_at_ms)
                    })
                    .map(|entry| entry.start_seq)
                    .max()
                    .unwrap_or(0)
            });
            pending.represented = self
                .queued_prompts
                .iter()
                .any(|queued| queued.id == pending.id)
                || self.entries.iter().any(|entry| {
                    entry
                        .source
                        .0
                        .as_ref()
                        .is_some_and(|item| represents_submission(&item.stable_id, &pending.id))
                });
        }
        self.pending_submissions
            .retain(|pending| !pending.represented);
        self.sync_inline_submissions();
    }

    pub(super) fn begin_submission(
        &mut self,
        id: String,
        kind: SubmissionKind,
        payload: PromptPayload,
        status: &str,
    ) {
        // A retry remains the same submission, including its original anchor.
        if let Some(pending) = self
            .pending_submissions
            .iter_mut()
            .find(|pending| pending.id == id)
        {
            pending.finished = false;
            pending.status = status.into();
            pending.started_at = Some(std::time::Instant::now());
            self.sync_inline_submissions();
            return;
        }
        self.pending_submissions.push(PendingSubmission {
            id,
            kind,
            payload,
            status: status.into(),
            startup_origin: None,
            represented: false,
            finished: false,
            recorded_at_ms: mj_core::clock::epoch_millis(),
            submitted_after: Some(self.latest_seq),
            started_at: Some(std::time::Instant::now()),
        });
        self.invalidate_render_cache();
    }

    pub(super) fn recoverable_submission_id(&self) -> Option<&str> {
        self.pending_submissions
            .iter()
            .rev()
            .find(|pending| pending.can_resend())
            .map(|pending| pending.id.as_str())
    }

    pub(super) fn dismiss_submission(&mut self, id: &str) {
        self.composer_runtime.receipt_preparations.remove(id);
        self.pending_submissions
            .retain(|pending| pending.id != id || !pending.can_resend());
        self.sync_inline_submissions();
    }

    pub(super) fn prepare_receipt_retry(&mut self, id: &str, session_id: &str) -> bool {
        if self.composer_runtime.receipt_preparations.contains_key(id) {
            return true;
        }
        let Some(receipt) = self
            .pending_submissions
            .iter_mut()
            .find(|pending| pending.id == id && pending.can_resend())
        else {
            return false;
        };
        if !receipt
            .payload
            .images
            .iter()
            .any(|image| image.image.reference.is_none())
        {
            return false;
        }
        let mut payload = receipt.payload.clone();
        let command_id = id.to_owned();
        let session_id = session_id.to_owned();
        let updates = self.composer_runtime.tx.clone();
        let task = tokio::spawn(async move {
            let result = async {
                for image in &mut payload.images {
                    if image.image.reference.is_none() {
                        image.image = composer::prepare_attachment(
                            &session_id,
                            composer::AttachmentSource::Clipboard(image.image.clone()),
                        )
                        .await?;
                    }
                }
                Ok(payload)
            }
            .await;
            if let Err(error) =
                updates.send(composer::ComposerIoUpdate::ReceiptPrepared { command_id, result })
            {
                tracing::debug!(%error, "receipt preparation result dropped because composer closed");
            }
        });
        self.composer_runtime
            .receipt_preparations
            .insert(id.to_owned(), composer::RemoteResultForwarder(Some(task)));
        self.sync_inline_submissions();
        true
    }

    pub(super) fn finish_receipt_preparation(
        &mut self,
        id: &str,
        result: Result<PromptPayload, String>,
    ) {
        self.composer_runtime.receipt_preparations.remove(id);
        let Some(receipt) = self
            .pending_submissions
            .iter_mut()
            .find(|pending| pending.id == id && pending.can_resend())
        else {
            return;
        };
        match result {
            Ok(payload) => {
                receipt.payload = payload;
                self.composer_runtime
                    .prepared_receipt_retries
                    .push_back(id.to_owned());
            }
            Err(error) => {
                receipt.status = format!("Delivery unconfirmed: image preparation failed: {error}")
            }
        }
        self.sync_inline_submissions();
    }

    pub(super) fn resend_submission(&self, id: &str) -> Option<remote::ChatRemoteOperation> {
        let pending = self
            .pending_submissions
            .iter()
            .find(|pending| pending.id == id && pending.can_resend())?;
        Some(match pending.kind {
            SubmissionKind::Prompt => remote::ChatRemoteOperation::Prompt {
                command_id: pending.id.clone(),
                text: pending.payload.text.clone(),
                images: pending.payload.images.clone(),
            },
            SubmissionKind::Shell => remote::ChatRemoteOperation::RunShell {
                command_id: pending.id.clone(),
                command: pending.payload.text.clone(),
            },
        })
    }

    pub(super) fn reconcile_submissions(&mut self, session: &MaterializedSession) {
        let mut inline_retired = false;
        for pending in &mut self.pending_submissions {
            if session
                .queued_prompts
                .iter()
                .any(|queued| queued.command_id == pending.id)
                || session
                    .transcript
                    .iter()
                    .any(|item| represents_submission(&item.stable_id, &pending.id))
            {
                if let Some(started) = pending.started_at.take() {
                    self.submission_renders.push((pending.id.clone(), started));
                }
                inline_retired |= pending.is_inline();
                pending.represented = true;
            }
        }
        self.pending_submissions
            .retain(|pending| !(pending.represented && pending.finished));
        if inline_retired {
            self.sync_inline_submissions();
        }
    }

    /// Returns false when authoritative state already confirmed this operation.
    pub(super) fn finish_submission(&mut self, id: &str, accepted: bool) -> bool {
        let Some(index) = self
            .pending_submissions
            .iter()
            .position(|pending| pending.id == id)
        else {
            return true;
        };
        let pending = &mut self.pending_submissions[index];
        let represented = pending.represented;
        pending.finished = true;
        pending.status = "Queued".into();
        // Once the relay has accepted a `/clear`, its "Clearing context…"
        // line, then its divider or its failure, record it in the
        // conversation. A row of our own would wait for a user message that
        // never comes.
        if represented || !accepted || pending.is_context_clear() {
            self.pending_submissions.remove(index);
        }
        self.sync_inline_submissions();
        !represented
    }

    pub(super) fn unconfirm_all_submissions(&mut self, error: &str) {
        for pending in &mut self.pending_submissions {
            if !pending.finished {
                pending.finished = true;
                pending.status = unconfirmed_status(error);
            }
        }
        self.pending_submissions
            .retain(|pending| !(pending.represented && pending.finished));
        self.sync_inline_submissions();
    }

    pub(super) fn unconfirm_submission(&mut self, id: &str, error: &str) {
        if let Some(pending) = self
            .pending_submissions
            .iter_mut()
            .find(|pending| pending.id == id)
        {
            pending.finished = true;
            pending.status = unconfirmed_status(error);
        }
        self.pending_submissions
            .retain(|pending| !(pending.represented && pending.finished));
        self.sync_inline_submissions();
    }

    /// Materialize local terminal receipts at their owner-recorded frontier.
    /// These entries never enter the durable projection or the pinned rows.
    pub(super) fn sync_inline_submissions(&mut self) {
        let position =
            (self.anchor != TranscriptAnchor::Bottom).then(|| self.transcript_position());
        let previous_len = self.entries.len();
        self.entries
            .retain(|entry| !is_inline_submission_entry(entry));
        let removed = self.entries.len() != previous_len;
        let recoverable = self.recoverable_submission_id();
        let inline = self
            .pending_submissions
            .iter()
            .filter(|pending| pending.is_inline())
            .map(|pending| {
                let text = if pending.kind == SubmissionKind::Shell {
                    format!("!{}", pending.payload.text)
                } else {
                    pending.payload.text.clone()
                };
                let status = if self
                    .composer_runtime
                    .receipt_preparations
                    .contains_key(&pending.id)
                {
                    "Delivery unconfirmed: preparing images…"
                } else {
                    &pending.status
                };
                let mut text = format!("{text}\n\n{status}");
                if !self.standby && recoverable == Some(pending.id.as_str()) {
                    text.push_str("\nCtrl-Alt-R resend · Ctrl-Alt-D dismiss");
                }
                let mut entry =
                    ChatEntry::plain(pending.submitted_after.unwrap_or(0), ChatRole::User, text)
                        .with_recorded_at(Some(pending.recorded_at_ms));
                entry.message_id = Some(format!("local-submission:{}", pending.id));
                entry
            })
            .collect::<Vec<_>>();
        if inline.is_empty() && !removed {
            self.invalidate_render_cache();
            return;
        }
        for entry in inline {
            let at = self
                .entries
                .partition_point(|existing| existing.start_seq <= entry.start_seq);
            self.entries.insert(at, entry);
        }
        self.invalidate_render_cache();
        if let Some(position) = position {
            self.restore_transcript_position(position);
        }
    }

    pub(super) fn has_inline_submissions(&self) -> bool {
        self.pending_submissions
            .iter()
            .any(PendingSubmission::is_inline)
    }

    pub(super) fn submission_status(&self, entry: &ChatEntry) -> Option<&str> {
        let id = entry.message_id.as_deref()?;
        self.pending_submissions
            .iter()
            .find(|pending| pending.id == id)
            .map(|pending| pending.status.as_str())
    }

    pub(super) fn submission_entries(&self) -> impl Iterator<Item = ChatEntry> + '_ {
        self.pending_submissions
            .iter()
            .filter(|pending| !pending.represented && !pending.is_inline())
            .map(|pending| {
                let text = if pending.kind == SubmissionKind::Shell {
                    format!("!{}", pending.payload.text)
                } else {
                    pending.payload.text.clone()
                };
                let mut entry = ChatEntry::plain(self.latest_seq, ChatRole::User, text)
                    .with_recorded_at(Some(pending.recorded_at_ms));
                entry.message_id = Some(pending.id.clone());
                entry
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::remote::{
        ChatRemoteOperation, ChatRemoteResult, apply_chat_remote_result,
        queue_chat_remote_operation,
    };
    use crate::chat::test_support::{snapshot, transcript_text};

    fn submit(chat: &mut ChatState, id: &str, text: &str) {
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        queue_chat_remote_operation(
            &tx,
            ChatRemoteOperation::Prompt {
                command_id: id.into(),
                text: text.into(),
                images: Vec::new(),
            },
            chat,
        );
    }

    fn result(chat: &mut ChatState, id: &str, result: Result<u64, String>) {
        apply_chat_remote_result(
            chat,
            ChatRemoteResult::Prompt {
                command_id: id.into(),
                text: "hello".into(),
                images: Vec::new(),
                result,
            },
        );
    }

    fn queued(id: &str) -> MaterializedSession {
        let mut session = MaterializedSession::empty("test");
        session.queued_prompts.push(MaterializedQueuedPrompt {
            command_id: id.into(),
            kind: QueuedCommandKind::Prompt,
            content: vec![serde_json::json!({"type":"text", "text":"hello"})],
            queued_at_ms: 1,
            accepted_ordinal: Some(1),
        });
        session
    }

    #[test]
    fn a_refused_local_resend_preserves_the_unconfirmed_receipt() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        queue_chat_remote_operation(
            &tx,
            ChatRemoteOperation::Prompt {
                command_id: "original".into(),
                text: "hello".into(),
                images: Vec::new(),
            },
            &mut chat,
        );
        chat.unconfirm_submission("original", "connection reset");
        chat.set_input("next draft".into());
        let retry = chat.resend_submission("original").unwrap();
        queue_chat_remote_operation(&tx, retry, &mut chat);
        assert_eq!(chat.recoverable_submission_id(), Some("original"));
        assert_eq!(chat.pending_submissions.len(), 1);
        assert_eq!(chat.input, "next draft");
    }

    // Hard-won: #1514: failed legacy receipt installation must remain retryable.
    #[tokio::test]
    async fn failed_receipt_preparation_preserves_bytes_identity_and_visible_draft() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let payload = PromptPayload::with_image("old receipt", crate::chat::tests::test_image());
        chat.begin_submission(
            "original".into(),
            SubmissionKind::Prompt,
            payload.clone(),
            "Sending…",
        );
        chat.unconfirm_submission("original", "connection reset");
        chat.set_input("new draft".into());
        assert!(chat.prepare_receipt_retry("original", "receipt-failure"));
        assert!(
            transcript_text(&mut chat, 100)
                .join("\n")
                .contains("preparing images")
        );
        assert!(!chat.encoded_draft().contains("preparing images"));
        chat.apply_composer_update(composer::ComposerIoUpdate::ReceiptPrepared {
            command_id: "original".into(),
            result: Err("install refused".into()),
        });
        assert_eq!(chat.input, "new draft");
        assert_eq!(chat.pending_submissions[0].payload, payload);
        assert!(
            chat.pending_submissions[0]
                .status
                .contains("install refused")
        );
        assert!(chat.composer_runtime.prepared_receipt_retries.is_empty());
        let saved = chat.encoded_draft();
        let mut reopened = ChatState::new(&snapshot(), &[]);
        reopened.restore_draft(saved);
        assert_eq!(reopened.recoverable_submission_id(), Some("original"));
        assert_eq!(reopened.pending_submissions[0].payload, payload);
        assert!(reopened.prepare_receipt_retry("original", "receipt-failure"));
    }

    #[test]
    fn submission_is_visible_before_any_remote_reply_and_keeps_the_new_draft() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "one", "hello");
        chat.set_input("next draft".into());
        let screen = transcript_text(&mut chat, 100).join("\n");
        assert!(screen.contains("hello"), "{screen}");
        assert!(screen.contains("Sending"), "{screen}");
        result(&mut chat, "one", Ok(42));
        let screen = transcript_text(&mut chat, 100).join("\n");
        assert!(
            screen.contains("hello") && screen.contains("Queued"),
            "{screen}"
        );
        assert_eq!(chat.input, "next draft");
        assert!(chat.notice().is_none());
        chat.apply_materialized(&queued("one"), &[], &[]);
        assert_eq!(chat.submission_entries().count(), 0);
        assert_eq!(chat.queued_prompts.len(), 1);
    }

    #[test]
    fn projection_before_reply_prevents_a_late_error_restoring_a_sent_prompt() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "one", "hello");
        chat.apply_materialized(&queued("one"), &[], &[]);
        chat.set_input("new draft".into());
        result(&mut chat, "one", Err("lost reply".into()));
        assert_eq!(chat.input, "new draft");
        assert!(chat.conversation_notices.is_empty());
        assert!(chat.pending_submissions.is_empty());
    }

    #[test]
    fn identical_prompts_reconcile_by_command_identity() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "one", "hello");
        submit(&mut chat, "two", "hello");
        result(&mut chat, "one", Ok(1));
        chat.apply_materialized(&queued("one"), &[], &[]);
        assert_eq!(chat.submission_entries().count(), 1);
        assert_eq!(chat.pending_submissions[0].id, "two");
    }

    #[test]
    fn an_unconfirmed_submission_survives_rebuild_and_saved_draft_restore() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "one", "hello");
        chat.unconfirm_submission("one", "connection lost");
        chat.set_input("new draft".into());
        chat.apply_materialized(&MaterializedSession::empty("test"), &[], &[]);
        let saved = chat.encoded_draft();
        let mut reopened = ChatState::new(&snapshot(), &[]);
        reopened.restore_draft(saved);
        assert_eq!(reopened.input, "new draft");
        let screen = transcript_text(&mut reopened, 100).join("\n");
        assert!(
            screen.contains("hello") && screen.contains("Delivery unconfirmed"),
            "{screen}"
        );
        reopened.apply_materialized(&queued("one"), &[], &[]);
        assert_eq!(reopened.submission_entries().count(), 0);
    }

    /// I1-12: the words "Delivery unconfirmed" appear once.
    // Hard-won: 8bf21ad0: The shipped refusal path repeated the Delivery unconfirmed prefix.
    #[test]
    fn an_unconfirmed_status_does_not_repeat_its_prefix() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "one", "hello");
        chat.unconfirm_submission("one", "delivery unconfirmed: channel closed");
        assert_eq!(
            chat.pending_submissions[0].status,
            "Delivery unconfirmed: channel closed"
        );
    }

    /// The divider the projection draws when the relay clears the context
    /// for command `id`, followed by one later turn.
    fn cleared_then_answered(id: &str) -> MaterializedSession {
        let mut session = MaterializedSession::empty("test");
        session.transcript.push(std::sync::Arc::new(TranscriptItem {
            stable_id: format!("{}{id}", mj_core::archive::CONTEXT_BOUNDARY_PREFIX),
            position: 2,
            latest_content_event_ordinal: None,
            created_at_ms: 2,
            last_changed_at_ms: 2,
            body: TranscriptBody::System {
                text: "Context cleared — a new conversation starts here.".into(),
            },
        }));
        session
            .transcript
            .push(crate::chat::test_support::agent_message_item(
                "agent:3", 3, "NONE",
            ));
        session
    }

    /// R4-4: an accepted `/clear` left "You · /clear · Queued" pinned below
    /// every later turn, and after resume the saved row read "Delivery
    /// unconfirmed". A clear has no user message to reconcile with; the
    /// relay's own "Clearing context…" line and divider are its record.
    // Hard-won: 1c0dc652: Accepted /clear rows stayed pinned and returned as unconfirmed after resume.
    #[test]
    fn an_accepted_clear_leaves_no_pending_row_or_saved_submission() {
        // The relay's acceptance arrives before the divider.
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "clear", "/clear");
        result(&mut chat, "clear", Ok(7));
        chat.apply_materialized(&cleared_then_answered("clear"), &[], &[]);
        let screen = transcript_text(&mut chat, 100).join("\n");
        assert!(!screen.contains("Queued"), "{screen}");
        assert!(chat.pending_submissions.is_empty());
        assert!(
            !chat.encoded_draft().contains("/clear"),
            "{}",
            chat.encoded_draft()
        );

        // The divider arrives before the acceptance.
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "clear", "/clear");
        chat.apply_materialized(&cleared_then_answered("clear"), &[], &[]);
        result(&mut chat, "clear", Ok(7));
        assert!(chat.pending_submissions.is_empty());

        // A draft an older build saved with the row still pending is not
        // replayed once the divider shows the clear ran.
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "clear", "/clear");
        chat.unconfirm_submission("clear", "connection lost");
        let saved = chat.encoded_draft();
        let mut reopened = ChatState::new(&snapshot(), &[]);
        reopened.restore_draft(saved);
        reopened.apply_materialized(&cleared_then_answered("clear"), &[], &[]);
        let screen = transcript_text(&mut reopened, 100).join("\n");
        assert!(!screen.contains("Delivery unconfirmed"), "{screen}");
        assert!(reopened.pending_submissions.is_empty());
    }

    /// I1-12: a refused `/clear` becomes a dated notice, leaves nothing pinned
    /// below later turns or saved with the draft, and keeps the command in
    /// the composer for a retry.
    // Hard-won: 8bf21ad0: A refused /clear was mislabeled unconfirmed and persisted as a pending row.
    #[test]
    fn a_refused_slash_command_is_a_notice_not_a_pinned_row() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "clear", "/clear");
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                command_id: "clear".into(),
                text: "/clear".into(),
                images: Vec::new(),
                result: Err("/clear requires an idle session".into()),
            },
        );
        assert!(chat.pending_submissions.is_empty());
        assert_eq!(chat.input, "/clear");
        assert_eq!(
            chat.conversation_notices
                .last()
                .map(|notice| notice.text.as_str()),
            Some("/clear was not run: /clear requires an idle session")
        );
        assert!(!chat.encoded_draft().contains("requires an idle session"));
    }

    #[test]
    fn refused_shell_keeps_content_in_the_conversation_and_restores_the_prefix() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.begin_submission(
            "shell".into(),
            SubmissionKind::Shell,
            PromptPayload::text("pwd"),
            "Sending…",
        );
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::RunShell {
                command_id: "shell".into(),
                command: "pwd".into(),
                result: Err("refused".into()),
            },
        );
        assert_eq!(chat.input, "!pwd");
        let screen = transcript_text(&mut chat, 100).join("\n");
        assert!(
            screen.contains("Shell command was not sent") && screen.contains("pwd"),
            "{screen}"
        );
        assert!(chat.submission_entries().next().is_none());
    }
    #[test]
    fn worker_failure_retires_sending_without_losing_the_payload() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        submit(&mut chat, "one", "hello");
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::WorkerFailed("worker stopped".into()),
        );
        let screen = transcript_text(&mut chat, 100).join("\n");
        assert!(
            screen.contains("hello") && screen.contains("Delivery unconfirmed"),
            "{screen}"
        );
        assert!(!screen.contains("Sending"), "{screen}");
        assert!(!screen.contains("was not sent"), "{screen}");
    }

    #[test]
    fn an_image_only_pending_submission_keeps_its_marker_and_image_on_refusal() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        let image = crate::clipboard::ClipboardImage {
            data_base64: "image-data".into(),
            mime_type: "image/png".into(),
            reference: None,
        };
        let payload = PromptPayload::with_image("", image);
        chat.begin_submission(
            "image".into(),
            SubmissionKind::Prompt,
            payload.clone(),
            "Sending…",
        );
        let screen = transcript_text(&mut chat, 100).join("\n");
        assert!(screen.contains("[image 1]"), "{screen}");
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                command_id: "image".into(),
                text: payload.text.clone(),
                images: payload.images.clone(),
                result: Err("refused".into()),
            },
        );
        assert_eq!(chat.draft_payload(), payload);
    }
}
