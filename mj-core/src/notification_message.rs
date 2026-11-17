//! Which message a notification card shows.
//!
//! The terminal pane and the web viewer both show the full text of the message
//! that needs the person. This module owns the rule that picks it, so the two
//! surfaces cannot drift: each supplies what it knows and resolves the sources
//! this returns, in order, to the first one that yields text.

/// The two notification levels whose card shows a message from the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    NeedsInput,
    Important,
}

/// What a surface knows about the session. Every field is optional because
/// each surface learns different things at different times.
#[derive(Debug, Clone, Copy)]
pub struct NotificationMessageFacts<'a> {
    /// The message of the newest pending form or permission request.
    pub pending_request: Option<&'a str>,
    /// What a running review says it is doing.
    pub review_activity: Option<&'a str>,
    /// Stable id of the transcript message that asks the question.
    pub question_message: Option<&'a str>,
    /// False for a harness that cannot raise an open request, so its question
    /// is only the last thing the agent wrote.
    pub supports_open_request: bool,
}

/// A place a card's text can come from. The caller resolves it; a source that
/// resolves to nothing falls through to the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationMessageSource<'a> {
    /// The text itself: a pending request's message.
    PendingRequest(&'a str),
    /// The text itself: a review's activity line.
    ReviewActivity(&'a str),
    /// The transcript message with this stable id.
    QuestionMessage(&'a str),
    /// The latest agent message of the session.
    LastAgentMessage,
}

/// The sources for a card, most specific first.
pub fn notification_message_sources<'a>(
    kind: NotificationKind,
    facts: &NotificationMessageFacts<'a>,
) -> Vec<NotificationMessageSource<'a>> {
    match kind {
        NotificationKind::Important => vec![NotificationMessageSource::LastAgentMessage],
        NotificationKind::NeedsInput => facts
            .pending_request
            .map(NotificationMessageSource::PendingRequest)
            .into_iter()
            .chain(
                facts
                    .review_activity
                    .map(NotificationMessageSource::ReviewActivity),
            )
            .chain(
                facts
                    .question_message
                    .map(NotificationMessageSource::QuestionMessage),
            )
            .chain(
                (!facts.supports_open_request)
                    .then_some(NotificationMessageSource::LastAgentMessage),
            )
            .collect(),
    }
}

/// What a card says when no source yields text.
pub fn notification_message_placeholder(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::NeedsInput => "Waiting for your input",
        NotificationKind::Important => "Important update",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_request_outranks_the_question_and_the_last_message_is_only_for_legacy_harnesses() {
        let mut facts = NotificationMessageFacts {
            pending_request: Some("Approve?"),
            review_activity: Some("Reviewing"),
            question_message: Some("agent:1"),
            supports_open_request: true,
        };
        assert_eq!(
            notification_message_sources(NotificationKind::NeedsInput, &facts),
            [
                NotificationMessageSource::PendingRequest("Approve?"),
                NotificationMessageSource::ReviewActivity("Reviewing"),
                NotificationMessageSource::QuestionMessage("agent:1"),
            ]
        );
        facts.supports_open_request = false;
        assert_eq!(
            notification_message_sources(NotificationKind::NeedsInput, &facts).last(),
            Some(&NotificationMessageSource::LastAgentMessage)
        );
        assert_eq!(
            notification_message_sources(NotificationKind::Important, &facts),
            [NotificationMessageSource::LastAgentMessage]
        );
    }
}
