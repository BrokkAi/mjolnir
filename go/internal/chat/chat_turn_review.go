// Port of mj-chat/src/chat/turn_review.rs.

// Passive status for a review in the parent conversation.

package chat

import (
	"iter"
	"reflect"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// TurnReview is one open review the daemon projected into this conversation.
type TurnReview struct {
	view    client.RuntimeReviewView
	failure *string
}

func newTurnReview(view client.RuntimeReviewView) TurnReview {
	return TurnReview{view: view}
}

// update replaces the view and reports whether it changed.
func (r *TurnReview) update(view client.RuntimeReviewView) bool {
	changed := !reflect.DeepEqual(r.view, view)
	r.view = view
	return changed
}

func (r *TurnReview) status() string {
	if r.failure != nil {
		return *r.failure
	}
	return r.view.Status
}

// reviewerElicitation is a pending form and the reviewer child session that
// owns it (Rust's (String, ElicitationRequest) pair).
type reviewerElicitation struct {
	Role    string
	Request core.ElicitationRequest
}

// pendingElicitations are the forms this review's own reviewers wait on,
// while the review is writing its brief or running.
func (r *TurnReview) pendingElicitations() []reviewerElicitation {
	switch r.view.Phase.(type) {
	case core.TurnReviewPhaseWritingBrief, core.TurnReviewPhaseRunning:
	default:
		return nil
	}
	var pending []reviewerElicitation
	for _, question := range r.view.Questions {
		if r.view.OwnsReviewer(question.Role) {
			pending = append(pending, reviewerElicitation{Role: question.Role, Request: question.Request})
		}
	}
	return pending
}

// reportFailure records message as the review's status, reporting whether
// that changed anything.
func (r *TurnReview) reportFailure(message string) bool {
	if r.failure != nil && *r.failure == message {
		return false
	}
	r.failure = &message
	return true
}

// TurnReviewIntent is what a review control asks for. chat.rs re-exports it
// as TurnReviewRequest.
type TurnReviewIntent int

const (
	TurnReviewIntentStart TurnReviewIntent = iota
)

// Rust's turn_reviews() getter, the session's open reviews in the order the
// daemon published them, is the turnReviews field itself: Go cannot give a
// method the name of a field.

// reviewViews are the reviews' own views, for the aggregations that read
// only them.
func (s *ChatState) reviewViews() iter.Seq[*client.RuntimeReviewView] {
	return func(yield func(*client.RuntimeReviewView) bool) {
		for index := range s.turnReviews {
			if !yield(&s.turnReviews[index].view) {
				return
			}
		}
	}
}

func (s *ChatState) hasTurnReview() bool {
	return len(s.turnReviews) > 0
}

// turnReviewStatus is the one line the footer shows for the reviews: the
// status of a single review, or each review's status after its repositories
// when there are several.
func (s *ChatState) turnReviewStatus() (string, bool) {
	switch len(s.turnReviews) {
	case 0:
		return "", false
	case 1:
		return s.turnReviews[0].status(), true
	}
	// One row has room for each review's repository and state, not for each
	// review's full status.
	parts := make([]string, 0, len(s.turnReviews))
	for index := range s.turnReviews {
		review := &s.turnReviews[index]
		state := "Reviewing"
		if review.failure != nil {
			state = *review.failure
		} else if label, ok := review.view.ActivityLabel(); ok {
			state = label
		}
		if roots := review.view.RootsLabel(); roots != "" {
			state = roots + ": " + state
		}
		parts = append(parts, state)
	}
	return strings.Join(parts, " · "), true
}

// reportTurnReviewFailure reports a refusal on the review it is about when
// exactly one is open; with several, a refusal cannot be tied to one of
// them, so it is a notice.
func (s *ChatState) reportTurnReviewFailure(message string) {
	if len(s.turnReviews) == 1 {
		s.turnReviews[0].reportFailure(message)
		return
	}
	s.SetNotice(message)
}

// setTurnReviews replaces the open reviews with the daemon's latest
// projection. A review that stays open keeps the failure it reported.
func (s *ChatState) setTurnReviews(views []client.RuntimeReviewView) {
	s.reviewProjectionKnown = true
	previous := s.turnReviews
	s.turnReviews = make([]TurnReview, 0, len(views))
	for _, view := range views {
		index := slices.IndexFunc(previous, func(open TurnReview) bool { return open.view.ReviewID == view.ReviewID })
		if index < 0 {
			s.turnReviews = append(s.turnReviews, newTurnReview(view))
			continue
		}
		// Rust's swap_remove: the last review takes the removed one's place.
		open := previous[index]
		last := len(previous) - 1
		previous[index] = previous[last]
		previous = previous[:last]
		open.update(view)
		s.turnReviews = append(s.turnReviews, open)
	}
}
