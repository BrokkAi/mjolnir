//! Local submission rows bridge input handling and the durable projection.
use super::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingSubmission {
    id: String,
    kind: UnsentKind,
    payload: PromptPayload,
    status: String,
    represented: bool,
    finished: bool,
    recorded_at_ms: i64,
    #[serde(skip)]
    started_at: Option<std::time::Instant>,
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

impl ChatState {
    pub(super) fn restore_submissions(&mut self, pending: Vec<PendingSubmission>) {
        self.pending_submissions = pending;
        for pending in &mut self.pending_submissions {
            pending.finished = true;
            pending.status = "Delivery unconfirmed".into();
            pending.represented = self
                .queued_prompts
                .iter()
                .any(|queued| queued.id == pending.id)
                || self.entries.iter().any(|entry| {
                    entry.source.0.as_ref().is_some_and(|item| {
                        item.stable_id == format!("user:{}", pending.id)
                            || item.stable_id == format!("shell:{}", pending.id)
                    })
                });
        }
        self.pending_submissions
            .retain(|pending| !pending.represented);
    }

    pub(super) fn begin_submission(
        &mut self,
        id: String,
        kind: UnsentKind,
        payload: PromptPayload,
        status: &str,
    ) {
        self.pending_submissions.push(PendingSubmission {
            id,
            kind,
            payload,
            status: status.into(),
            represented: false,
            finished: false,
            recorded_at_ms: mj_core::clock::epoch_millis(),
            started_at: Some(std::time::Instant::now()),
        });
        self.anchor = TranscriptAnchor::Bottom;
        self.invalidate_render_cache();
    }

    pub(super) fn reconcile_submissions(&mut self, session: &MaterializedSession) {
        for pending in &mut self.pending_submissions {
            if session
                .queued_prompts
                .iter()
                .any(|queued| queued.command_id == pending.id)
                || session.transcript.iter().any(|item| {
                    item.stable_id == format!("user:{}", pending.id)
                        || item.stable_id == format!("shell:{}", pending.id)
                })
            {
                if let Some(started) = pending.started_at.take() {
                    self.submission_renders.push((pending.id.clone(), started));
                }
                pending.represented = true;
            }
        }
        self.pending_submissions
            .retain(|pending| !(pending.represented && pending.finished));
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
        if represented || !accepted {
            self.pending_submissions.remove(index);
        }
        self.invalidate_render_cache();
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
        self.invalidate_render_cache();
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
        self.invalidate_render_cache();
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
            .filter(|pending| !pending.represented)
            .map(|pending| {
                let text = if pending.kind == UnsentKind::Shell {
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
        assert!(chat.unsent_prompts.is_empty());
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

    /// I1-12: a refused `/clear` becomes a dated notice, leaves nothing pinned
    /// below later turns or saved with the draft, and keeps the command in
    /// the composer for a retry.
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
        assert!(chat.unsent_prompts.is_empty());
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
            UnsentKind::Shell,
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
        assert!(chat.unsent_prompts.is_empty());
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
            UnsentKind::Prompt,
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
