//! Conversation feedback stays separate from dashboard-wide notifications.
use super::*;

#[derive(Debug)]
pub(super) struct ConversationNotice {
    pub id: String,
    pub text: String,
    pub submitted: bool,
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
