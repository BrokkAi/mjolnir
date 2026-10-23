//! Conversation feedback stays separate from dashboard-wide notifications.
use super::*;

#[derive(Debug)]
pub(super) struct ConversationNotice {
    pub id: String,
    pub text: String,
    pub submitted: bool,
    /// The journal ordinal this chat had applied when it last sent the notice.
    pub sent_at: u64,
    /// The relay refused the last send.
    pub refused: bool,
    pub recorded_at_ms: i64,
}

impl ChatState {
    pub(super) fn conversation_notice(&mut self, text: impl Into<String>) {
        let text = sanitize_terminal_text(&text.into());
        if self
            .conversation_notices
            .last()
            .is_some_and(|notice| notice.text == text)
        {
            return;
        }
        let id = match mj_client::session::new_command_id("chat-notice") {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(%error, "could not identify conversation notice");
                String::new()
            }
        };
        self.conversation_notices.push(ConversationNotice {
            submitted: id.is_empty(),
            id,
            text,
            sent_at: 0,
            refused: false,
            recorded_at_ms: mj_core::clock::epoch_millis(),
        });
        self.invalidate_render_cache();
    }

    pub(super) fn reconcile_notices(&mut self, session: &MaterializedSession) {
        self.conversation_notices.retain(|notice| {
            !session
                .transcript
                .iter()
                .any(|item| item.stable_id == format!("system:notice:{}", notice.id))
        });
        self.resend_refused_notices(session.applied_event_ordinal);
    }

    /// The relay refuses every command while it clears context, notices
    /// included. A refused notice is sent again once the journal has moved past
    /// the point where it was sent, so it reaches the conversation when the
    /// clear ends instead of staying a local row below every later turn.
    pub(super) fn refuse_notice(&mut self, id: &str) {
        if let Some(notice) = self
            .conversation_notices
            .iter_mut()
            .find(|notice| notice.id == id)
        {
            notice.refused = true;
        }
        self.resend_refused_notices(self.latest_seq);
    }

    fn resend_refused_notices(&mut self, applied_ordinal: u64) {
        for notice in &mut self.conversation_notices {
            if notice.refused && applied_ordinal > notice.sent_at {
                notice.refused = false;
                notice.submitted = false;
            }
        }
    }

    pub(super) fn notice_entries(&self) -> impl Iterator<Item = ChatEntry> + '_ {
        self.conversation_notices.iter().map(|notice| {
            ChatEntry::plain(self.latest_seq, ChatRole::System, &notice.text)
                .with_recorded_at(Some(notice.recorded_at_ms))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_support::{snapshot, transcript_text};

    #[test]
    fn local_validation_does_not_replace_dashboard_notifications() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.notices.set("Dashboard notification");
        chat.set_notice("usage: /model <value>");
        assert_eq!(
            chat.notices.current().as_deref(),
            Some("Dashboard notification")
        );
        assert_eq!(chat.notice().as_deref(), Some("usage: /model <value>"));
        chat.set_input("new draft".into());
        assert!(chat.notice().is_none());
    }

    #[test]
    fn a_conversation_failure_survives_projection_rebuild_without_repeating() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.conversation_notice("Cancellation failed: disconnected");
        chat.conversation_notice("Cancellation failed: disconnected");
        chat.apply_materialized(&MaterializedSession::empty("test"), &[], &[]);
        let screen = transcript_text(&mut chat, 100).join("\n");
        assert_eq!(screen.matches("Cancellation failed").count(), 1, "{screen}");
        assert!(chat.notice().is_none());
    }
}
