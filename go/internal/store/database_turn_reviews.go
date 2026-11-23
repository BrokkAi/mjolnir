// Port of mj-store/src/database/turn_reviews.rs.

// Durable review state and the finished report each review leaves behind.
//
// The review host owns every decision about a review, including which text
// settled a role. This module stores those decisions; it never re-derives one
// from another table, because the reviewer child's mutable rows are pruned
// when its role ends and can be rewritten by a later command.

package store

import (
	"encoding/json"
	"errors"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// AcceptedReport is one reviewing role's report, as the review host settled
// it.
//
// The host passes the exact text it accepted, so a later handback from the
// same child cannot become this review's output. It names the review, so a
// report cannot land on another open review that has a role of the same
// name.
type AcceptedReport struct {
	ReviewID string
	Role     string
	Text     string
}

// FinishedReview is one finished review and the output of each reviewing
// role. It is stored separately from the reviewer child sessions, which are
// destroyed as soon as their role is over.
type FinishedReview struct {
	SessionID    string                        `json:"session_id"`
	ReviewID     string                        `json:"review_id"`
	FinishedAtMs int64                         `json:"finished_at_ms"`
	Resolution   core.FinishedReviewResolution `json:"resolution"`
	Roles        []FinishedReviewRole          `json:"roles"`
}

// MarshalJSON writes a nil Roles as [] (a present, empty Vec).
func (r FinishedReview) MarshalJSON() ([]byte, error) {
	type plain FinishedReview
	out := plain(r)
	if out.Roles == nil {
		out.Roles = []FinishedReviewRole{}
	}
	return json.Marshal(out)
}

// UnmarshalJSON requires every field, as serde does.
func (r *FinishedReview) UnmarshalJSON(data []byte) error {
	var raw struct {
		SessionID    *string                        `json:"session_id"`
		ReviewID     *string                        `json:"review_id"`
		FinishedAtMs *int64                         `json:"finished_at_ms"`
		Resolution   *core.FinishedReviewResolution `json:"resolution"`
		Roles        *[]FinishedReviewRole          `json:"roles"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	switch {
	case raw.SessionID == nil:
		return errors.New("missing field `session_id`")
	case raw.ReviewID == nil:
		return errors.New("missing field `review_id`")
	case raw.FinishedAtMs == nil:
		return errors.New("missing field `finished_at_ms`")
	case raw.Resolution == nil:
		return errors.New("missing field `resolution`")
	case raw.Roles == nil:
		return errors.New("missing field `roles`")
	}
	*r = FinishedReview{
		SessionID:    *raw.SessionID,
		ReviewID:     *raw.ReviewID,
		FinishedAtMs: *raw.FinishedAtMs,
		Resolution:   *raw.Resolution,
		Roles:        *raw.Roles,
	}
	return nil
}

// FinishedReviewRole is one role's output from a finished review, including
// the turn it ran for usage. Turn is read from the accounting retained after
// session cleanup.
type FinishedReviewRole struct {
	Role           string  `json:"role"`
	Label          string  `json:"label"`
	CommandID      string  `json:"command_id"`
	ChildSessionID *string `json:"child_session_id,omitempty"`
	Profile        string  `json:"profile"`
	Model          *string `json:"model,omitempty"`
	Effort         *string `json:"effort,omitempty"`
	// Handback is the text this role's outcome was settled from, when it
	// handed back.
	Handback *string                       `json:"handback,omitempty"`
	Verdict  core.ReviewVerdict            `json:"verdict,omitempty"`
	Turn     *core.MaterializedTurnOutcome `json:"turn,omitempty"`
}

// UnmarshalJSON requires the fields serde requires and decodes the verdict
// through its owner's decoder.
func (r *FinishedReviewRole) UnmarshalJSON(data []byte) error {
	var raw struct {
		Role           *string                       `json:"role"`
		Label          *string                       `json:"label"`
		CommandID      *string                       `json:"command_id"`
		ChildSessionID *string                       `json:"child_session_id"`
		Profile        *string                       `json:"profile"`
		Model          *string                       `json:"model"`
		Effort         *string                       `json:"effort"`
		Handback       *string                       `json:"handback"`
		Verdict        json.RawMessage               `json:"verdict"`
		Turn           *core.MaterializedTurnOutcome `json:"turn"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	switch {
	case raw.Role == nil:
		return errors.New("missing field `role`")
	case raw.Label == nil:
		return errors.New("missing field `label`")
	case raw.CommandID == nil:
		return errors.New("missing field `command_id`")
	case raw.Profile == nil:
		return errors.New("missing field `profile`")
	}
	role := FinishedReviewRole{
		Role:           *raw.Role,
		Label:          *raw.Label,
		CommandID:      *raw.CommandID,
		ChildSessionID: raw.ChildSessionID,
		Profile:        *raw.Profile,
		Model:          raw.Model,
		Effort:         raw.Effort,
		Handback:       raw.Handback,
		Turn:           raw.Turn,
	}
	if raw.Verdict != nil && string(raw.Verdict) != "null" {
		verdict, err := core.UnmarshalReviewVerdict(raw.Verdict)
		if err != nil {
			return err
		}
		role.Verdict = verdict
	}
	*r = role
	return nil
}

// TurnReviewState is how far sessionID has been reviewed, or a fresh state
// when it has never been reviewed.
func TurnReviewState(sessionID string) (core.TurnReviewState, error) {
	return turnReviewStateIn(DatabasePath(), sessionID)
}

// ActiveReviewGroupState is a session with review groups whose reviewer
// workers must be reattached after daemon handoff (Rust's (String,
// TurnReviewState)).
type ActiveReviewGroupState struct {
	SessionID string
	State     core.TurnReviewState
}

// ActiveReviewGroups is the groups whose reviewer workers must be reattached
// after daemon handoff.
func ActiveReviewGroups() ([]ActiveReviewGroupState, error) {
	return activeReviewGroupsIn(DatabasePath())
}

func activeReviewGroupsIn(path string) ([]ActiveReviewGroupState, error) {
	panic("unported: mj-store/src/database/turn_reviews.rs active_review_groups_in")
}

func turnReviewStateIn(path, sessionID string) (core.TurnReviewState, error) {
	panic("unported: mj-store/src/database/turn_reviews.rs turn_review_state_in")
}

// storedOpenReviews is the stored open_reviews list for state: the reviews
// the row still has something to say about.
func storedOpenReviews(state *core.TurnReviewState) (string, error) {
	open := make([]*core.OpenReview, 0, len(state.OpenReviews))
	for index := range state.OpenReviews {
		if state.OpenReviews[index].IsStored() {
			open = append(open, &state.OpenReviews[index])
		}
	}
	encoded, err := json.Marshal(open)
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

// SaveTurnReviewState records review state and atomically queues newly
// settled findings while other reviewers are pending.
func SaveTurnReviewState(sessionID string, state *core.TurnReviewState) error {
	panic("unported: mj-store/src/database/turn_reviews.rs save_turn_review_state")
}

// SaveReviewerOutcome records review state together with the reports the
// host settled roles from. A save that carries no reports keeps the text
// already stored.
func SaveReviewerOutcome(sessionID string, state *core.TurnReviewState, reports []AcceptedReport) error {
	panic("unported: mj-store/src/database/turn_reviews.rs save_reviewer_outcome")
}

// CloseTurnReviewState clears the review reviewID from the row and records
// its per-role output in the same transaction. The resolution is explicit
// because a cancelled review's stored group can still carry the verdict it
// was about to deliver.
func CloseTurnReviewState(sessionID, reviewID string, state *core.TurnReviewState, resolution core.FinishedReviewResolution) error {
	panic("unported: mj-store/src/database/turn_reviews.rs close_turn_review_state")
}

func saveTurnReviewStateIn(path, sessionID string, state *core.TurnReviewState) error {
	return saveTurnReviewStateInWith(path, sessionID, state, nil, nil)
}

// saveReviewerOutcomeIn is SaveReviewerOutcome at an explicit store path.
func saveReviewerOutcomeIn(path, sessionID string, state *core.TurnReviewState, reports []AcceptedReport) error {
	return saveTurnReviewStateInWith(path, sessionID, state, nil, reports)
}

func closeTurnReviewStateIn(path, sessionID, reviewID string, state *core.TurnReviewState, resolution core.FinishedReviewResolution) error {
	return saveTurnReviewStateInWith(path, sessionID, state, &reviewClose{reviewID: reviewID, resolution: resolution}, nil)
}

// reviewClose is the review a save closes and how it ended.
type reviewClose struct {
	reviewID   string
	resolution core.FinishedReviewResolution
}

func saveTurnReviewStateInWith(path, sessionID string, state *core.TurnReviewState, close *reviewClose, reports []AcceptedReport) error {
	panic("unported: mj-store/src/database/turn_reviews.rs save_turn_review_state_in_with")
}

func previousActiveGroups(conn Queryer, sessionID string) ([]core.ActiveReviewGroup, error) {
	panic("unported: mj-store/src/database/turn_reviews.rs previous_active_groups")
}

// recordReviewReports keeps the durable per-role report in step with review
// state.
//
// reports carries the text the host settled each role from, with the review
// it belongs to. The store writes exactly that and nothing else: it never
// reads the reviewer child's mutable handback row, whose latest entry can
// belong to another command and would pair unrelated text with a settled
// verdict.
func recordReviewReports(conn Queryer, sessionID string, previous []core.ActiveReviewGroup, state *core.TurnReviewState, close *reviewClose, reports []AcceptedReport) error {
	panic("unported: mj-store/src/database/turn_reviews.rs record_review_reports")
}

// previousResolution is the resolution for a review that is being cleared.
// The host's explicit decision wins: a cancelled review's stored group can
// still carry the verdict it was about to deliver, and a delivery failure can
// arrive after a clean verdict was recorded. Only a close with no decision,
// which no production path produces, reads the driver's own outcome.
func previousResolution(previous *core.ActiveReviewGroup, close *reviewClose) core.FinishedReviewResolution {
	if close != nil && close.reviewID == previous.Driver.ReviewID {
		return close.resolution
	}
	return inferredResolution(previous)
}

// inferredResolution reads a review whose state is cleared without an
// explicit close: a delivered verdict means completed or failed, and no
// verdict means the review was cancelled.
func inferredResolution(group *core.ActiveReviewGroup) core.FinishedReviewResolution {
	panic("unported: mj-store/src/database/turn_reviews.rs inferred_resolution")
}

func syncReviewReport(conn Queryer, sessionID string, group *core.ActiveReviewGroup, reports []*AcceptedReport) error {
	panic("unported: mj-store/src/database/turn_reviews.rs sync_review_report")
}

func finalizeReviewReport(conn Queryer, sessionID string, group *core.ActiveReviewGroup, resolution core.FinishedReviewResolution) error {
	panic("unported: mj-store/src/database/turn_reviews.rs finalize_review_report")
}

func resolutionText(resolution core.FinishedReviewResolution) string {
	switch resolution {
	case core.FinishedReviewResolutionCompleted:
		return "completed"
	case core.FinishedReviewResolutionFailed:
		return "failed"
	case core.FinishedReviewResolutionCancelled:
		return "cancelled"
	}
	panic(fmt.Sprintf("unknown review resolution %q", string(resolution)))
}

func resolutionFromText(text string) (core.FinishedReviewResolution, error) {
	switch text {
	case "completed":
		return core.FinishedReviewResolutionCompleted, nil
	case "failed":
		return core.FinishedReviewResolutionFailed, nil
	case "cancelled":
		return core.FinishedReviewResolutionCancelled, nil
	}
	return "", fmt.Errorf("invalid review report resolution %q", text)
}

// FinishedReviewOf is the latest finished review for sessionID, or the one
// reviewID names (Rust's finished_review, named apart from the type).
func FinishedReviewOf(sessionID string, reviewID *string) (*FinishedReview, error) {
	return finishedReviewIn(DatabasePath(), sessionID, reviewID)
}

func finishedReviewIn(path, sessionID string, reviewID *string) (*FinishedReview, error) {
	panic("unported: mj-store/src/database/turn_reviews.rs finished_review_in")
}

// reviewerTurn is the one review turn a reviewer child ran. Accounting
// outlives the child's session row, which is why this is still readable
// after the child is destroyed.
func reviewerTurn(conn Queryer, child *string) (*core.MaterializedTurnOutcome, error) {
	panic("unported: mj-store/src/database/turn_reviews.rs reviewer_turn")
}

func saveTurnReviewStateWith(conn Queryer, sessionID string, state *core.TurnReviewState) error {
	panic("unported: mj-store/src/database/turn_reviews.rs save_turn_review_state_with")
}

// ClearInterruptedTurnReviews clears legacy interrupted reviews while
// retaining durable reviewer groups.
//
// Durable push groups resume independently. Preparation markers without a
// group are cleared and reported so the user can request review again.
func ClearInterruptedTurnReviews() ([]string, error) {
	panic("unported: mj-store/src/database/turn_reviews.rs clear_interrupted_turn_reviews")
}

func clearInterruptedTurnReviewsIn(path string) ([]string, error) {
	panic("unported: mj-store/src/database/turn_reviews.rs clear_interrupted_turn_reviews_in")
}
