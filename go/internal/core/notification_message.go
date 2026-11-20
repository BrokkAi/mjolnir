// Port of mj-core/src/notification_message.rs.
//
// Which message a notification card shows.
//
// The terminal pane and the web viewer both show the full text of the
// message that needs the person. This module owns the rule that picks it,
// so the two surfaces cannot drift: each supplies what it knows and
// resolves the sources this returns, in order, to the first one that yields
// text. (NotificationKind is declared in attention.go.)

package core

// NotificationMessageFacts is what a surface knows about the session. Every
// field is optional because each surface learns different things at
// different times.
type NotificationMessageFacts struct {
	// PendingRequest is the message of the newest pending form or permission
	// request.
	PendingRequest *string
	// ReviewActivity is what a running review says it is doing.
	ReviewActivity *string
	// QuestionMessage is the stable id of the transcript message that asks
	// the question.
	QuestionMessage *string
	// SupportsOpenRequest is false for a harness that cannot raise an open
	// request, so its question is only the last thing the agent wrote.
	SupportsOpenRequest bool
}

// NotificationMessageSource is a place a card's text can come from. The
// caller resolves it; a source that resolves to nothing falls through to
// the next one.
type NotificationMessageSource interface {
	isNotificationMessageSource()
}

// NotificationMessageSourcePendingRequest is the text itself: a pending
// request's message.
type NotificationMessageSourcePendingRequest struct{ Value string }

// NotificationMessageSourceReviewActivity is the text itself: a review's
// activity line.
type NotificationMessageSourceReviewActivity struct{ Value string }

// NotificationMessageSourceQuestionMessage is the transcript message with
// this stable id.
type NotificationMessageSourceQuestionMessage struct{ Value string }

// NotificationMessageSourceLastAgentMessage is the latest agent message of
// the session.
type NotificationMessageSourceLastAgentMessage struct{}

func (NotificationMessageSourcePendingRequest) isNotificationMessageSource()   {}
func (NotificationMessageSourceReviewActivity) isNotificationMessageSource()   {}
func (NotificationMessageSourceQuestionMessage) isNotificationMessageSource()  {}
func (NotificationMessageSourceLastAgentMessage) isNotificationMessageSource() {}

// NotificationMessageSources is the sources for a card, most specific first.
func NotificationMessageSources(kind NotificationKind, facts *NotificationMessageFacts) []NotificationMessageSource {
	if kind == NotificationKindImportant {
		return []NotificationMessageSource{NotificationMessageSourceLastAgentMessage{}}
	}
	sources := []NotificationMessageSource{}
	if facts.PendingRequest != nil {
		sources = append(sources, NotificationMessageSourcePendingRequest{*facts.PendingRequest})
	}
	if facts.ReviewActivity != nil {
		sources = append(sources, NotificationMessageSourceReviewActivity{*facts.ReviewActivity})
	}
	if facts.QuestionMessage != nil {
		sources = append(sources, NotificationMessageSourceQuestionMessage{*facts.QuestionMessage})
	}
	if !facts.SupportsOpenRequest {
		sources = append(sources, NotificationMessageSourceLastAgentMessage{})
	}
	return sources
}

// NotificationMessagePlaceholder is what a card says when no source yields text.
func NotificationMessagePlaceholder(kind NotificationKind) string {
	if kind == NotificationKindNeedsInput {
		return "Waiting for your input"
	}
	return "Important update"
}
