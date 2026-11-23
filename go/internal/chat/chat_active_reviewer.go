// Port of mj-chat/src/chat/active/reviewer.rs.

package chat

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// answerReviewer answers a form the reviewer's harness is waiting on.
func (c *ActiveChat) answerReviewer(role *string, elicitationID string, response core.ElicitationResponse, draft *ChatElicitationDraft) {
	if role == nil {
		if draft != nil {
			err := "The reviewer for this question is no longer available"
			c.restoreFailedReviewerElicitation(*draft, err)
			c.ReportReviewRefusal(err)
		}
		return
	}
	child := *role
	ownsQuestion := false
	for index := range c.turnReviews {
		view := &c.turnReviews[index].view
		if !view.OwnsReviewer(child) {
			continue
		}
		for _, question := range view.Questions {
			if question.Role == child && question.Request.ID == elicitationID {
				ownsQuestion = true
				break
			}
		}
		if ownsQuestion {
			break
		}
	}
	if !ownsQuestion {
		err := "The reviewer's question is no longer active"
		if draft != nil {
			c.restoreFailedReviewerElicitation(*draft, err)
		}
		c.ReportReviewRefusal(err)
		return
	}
	queueChatRemoteOperation(c.remote.operations(), chatRemoteOperationReviewerAnswer{
		Role:      child,
		RequestID: elicitationID,
		Response:  response,
		Draft:     draft,
	}, c.ChatState)
}

// surfaceReviewerElicitations puts a form the reviewer is waiting on in
// front of the user, or answers it for them when it is not theirs to answer.
func (c *ActiveChat) surfaceReviewerElicitations() {
	var pending []ReviewerPendingElicitation
	for index := range c.turnReviews {
		for _, question := range c.turnReviews[index].pendingElicitations() {
			role := question.Role
			pending = append(pending, ReviewerPendingElicitation{Role: &role, Request: question.Request})
		}
	}
	c.reconcileReviewerElicitation(pending)
	for _, question := range pending {
		role, request := question.Role, question.Request
		if c.reviewerAnswerPending(role, &request) {
			continue
		}
		// A reviewer's plan decision is the reviewer proposing work, not the
		// plan under review. It is never shown as the primary's decision;
		// the reviewer was asked to critique, not to implement, so it is
		// declined and its critique stands as the answer.
		if core.IsPlanReviewID(request.ID) {
			c.answerReviewer(role, request.ID, core.PlanReviewKeepPlanning(), nil)
			continue
		}
		if c.reviewerElicitationOpen() {
			c.applyDeferredElicitationDraft()
			c.applyDeferredFormResults()
			return
		}
		if !c.showReviewRoleElicitation(role, request) {
			c.applyDeferredElicitationDraft()
			c.applyDeferredFormResults()
			return
		}
		c.applyDeferredElicitationDraft()
	}
	c.applyDeferredElicitationDraft()
	c.applyDeferredFormResults()
}
