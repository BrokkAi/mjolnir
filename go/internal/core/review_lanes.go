// Port of mj-core/src/review/lanes.rs (and the doc of review.rs).

// Shared review contracts and status: shared review configuration, messages
// and persisted evidence.

package core

// UserMessage is one user-authored message captured from the primary
// session, in chronological order.
type UserMessage struct {
	Text string `json:"text"`
}

func UserMessagePrompt(text string) UserMessage {
	return UserMessage{Text: text}
}
