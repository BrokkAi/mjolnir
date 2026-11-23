// Port of mj-chat/src/chat/active/reviewer.rs.

package chat

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// answerReviewer answers a form the reviewer's harness is waiting on.
func (c *ActiveChat) answerReviewer(role *string, elicitationID string, response core.ElicitationResponse, draft *ChatElicitationDraft) {
	panic("unported: mj-chat/src/chat/active/reviewer.rs answerReviewer")
}

// surfaceReviewerElicitations puts a form the reviewer is waiting on in
// front of the user, or answers it for them when it is not theirs to answer.
func (c *ActiveChat) surfaceReviewerElicitations() {
	panic("unported: mj-chat/src/chat/active/reviewer.rs surfaceReviewerElicitations")
}
