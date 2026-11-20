// Port of mj-client/src/review.rs.

// Review data shared by Mjolnir's control surfaces.

package client

import (
	"iter"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RuntimeReviewView is what the host tells a surface about one running
// review.
type RuntimeReviewView struct {
	ChildSessionID *string              `json:"child_session_id,omitempty" serde:"default"`
	SessionID      string               `json:"session_id"`
	Phase          core.TurnReviewPhase `json:"phase"`
	Roles          []core.RoleStatus    `json:"roles"`
	// Status is what the review is doing, in one line.
	Status string `json:"status"`
	// Verdict is present once the review has reached a verdict.
	Verdict *VerdictView `json:"verdict"`
	// Questions are forms a reviewing harness is waiting for a person to
	// answer. The child owns them; the host projects its session's pending
	// requests.
	Questions []ReviewerQuestion `json:"questions,omitempty" serde:"default"`
}

func (v RuntimeReviewView) MarshalJSON() ([]byte, error) {
	type plain RuntimeReviewView
	return daemonMarshalStruct(plain(v))
}

func (v *RuntimeReviewView) UnmarshalJSON(data []byte) error {
	type plain RuntimeReviewView
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// ReviewerQuestion is a form and the reviewer child session that owns it.
type ReviewerQuestion struct {
	Role    string                  `json:"role"`
	Request core.ElicitationRequest `json:"request"`
}

func (v ReviewerQuestion) MarshalJSON() ([]byte, error) {
	type plain ReviewerQuestion
	return daemonMarshalStruct(plain(v))
}

func (v *ReviewerQuestion) UnmarshalJSON(data []byte) error {
	type plain ReviewerQuestion
	return daemonDecodeStruct(data, (*plain)(v), true)
}

// reviewerQuestionPrefix marks an elicitation id as a reviewing role's form
// rather than the primary agent's. Public ids allow only letters, digits,
// `-`, `_` and `.`, so the separator is a dot.
const reviewerQuestionPrefix = "review."

// SessionRequest is the request as the session's own question list carries
// it. Its id names the role, so an answer goes back to the harness that
// asked.
func (q ReviewerQuestion) SessionRequest() core.ElicitationRequest {
	request := q.Request
	request.ID = reviewerQuestionPrefix + q.Role + "." + q.Request.ID
	return request
}

// ParseReviewerQuestionID is the role and that role's own elicitation id
// behind an id made by ReviewerQuestion.SessionRequest; false for any other
// id.
func ParseReviewerQuestionID(id string) (role, inner string, ok bool) {
	rest, found := strings.CutPrefix(id, reviewerQuestionPrefix)
	if !found {
		return "", "", false
	}
	role, inner, found = strings.Cut(rest, ".")
	if !found || role == "" || inner == "" {
		return "", "", false
	}
	return role, inner, true
}

// ReviewerChildren is the child sessions in the host's stable reviewer
// order. A role without an assigned child is status only and has no
// transcript to attach to.
func (v RuntimeReviewView) ReviewerChildren() iter.Seq[string] {
	return func(yield func(string) bool) {
		for _, role := range v.Roles {
			if role.ChildSessionID != nil && !yield(*role.ChildSessionID) {
				return
			}
		}
	}
}

func (v RuntimeReviewView) OwnsReviewer(child string) bool {
	for id := range v.ReviewerChildren() {
		if id == child {
			return true
		}
	}
	return false
}

// IsWorking reports whether progress indicators should move. A verdict and a
// failed handoff wait for the user even though they retain an activity
// label.
func (v RuntimeReviewView) IsWorking() bool {
	// A reviewer waiting on a person's answer makes no progress on its own.
	if len(v.Questions) > 0 {
		return false
	}
	switch v.Phase.(type) {
	case core.TurnReviewPhaseCapturingDelta, core.TurnReviewPhaseLaunchingReviewer,
		core.TurnReviewPhaseWritingBrief, core.TurnReviewPhaseRunning:
		return true
	}
	return false
}

// ActivityLabel is a compact activity label for session lists and headers;
// false for none. It reads typed state rather than matching the driver's
// human-facing progress text.
func (v RuntimeReviewView) ActivityLabel() (string, bool) {
	switch phase := v.Phase.(type) {
	case core.TurnReviewPhaseResolved:
		return "", false
	case core.TurnReviewPhaseVerdict:
		if len(v.Questions) > 0 {
			return "Question", true
		}
		switch phase.Verdict.(type) {
		case core.ReviewVerdictFindings:
			return "Findings", true
		case core.ReviewVerdictFailed:
			return "Review failed", true
		}
		return "Review complete", true
	}
	if len(v.Questions) > 0 {
		return "Question", true
	}
	return "Reviewing", true
}

// VerdictView is a verdict as a surface renders it.
type VerdictView struct {
	Kind VerdictKind `json:"kind"`
	// Text is the findings, or the failure's reason. Empty for a clean
	// verdict, which resolves itself and is never on screen.
	Text string `json:"text"`
	// Allowed is legacy resolution data retained for terminal handoff
	// compatibility.
	Allowed []core.Resolution `json:"allowed"`
}

func (v VerdictView) MarshalJSON() ([]byte, error) {
	type plain VerdictView
	return daemonMarshalStruct(plain(v))
}

func (v *VerdictView) UnmarshalJSON(data []byte) error {
	type plain VerdictView
	return daemonDecodeStruct(data, (*plain)(v), true)
}

type VerdictKind string

const (
	VerdictKindClean    VerdictKind = "clean"
	VerdictKindFindings VerdictKind = "findings"
	VerdictKindFailed   VerdictKind = "failed"
)

func (k *VerdictKind) UnmarshalText(text []byte) error {
	switch value := VerdictKind(text); value {
	case VerdictKindClean, VerdictKindFindings, VerdictKindFailed:
		*k = value
		return nil
	}
	return daemonUnknownVariant(string(text), []string{"clean", "findings", "failed"})
}
