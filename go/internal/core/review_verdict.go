// Port of mj-core/src/review/verdict.rs.

// What a review concluded, and how a reviewing agent's reply is classified.
//
// The durable verdict and compatibility evidence shared by review drivers,
// storage and client projections.

package core

import (
	"encoding/json"
	"fmt"
)

// ReviewVerdict is what one review concluded. Internally tagged by
// "verdict" in snake_case.
type ReviewVerdict interface {
	isReviewVerdict()
	// IsClean reports whether this verdict resolves the review with nothing
	// to forward.
	IsClean() bool
	// IsComplete reports whether every assigned reviewer reached a usable
	// verdict for coverage.
	IsComplete() bool
}

// ReviewVerdictFindings is findings the user can forward to the primary agent.
type ReviewVerdictFindings struct {
	Synthesis string             `json:"synthesis"`
	Evidence  ReviewPassEvidence `json:"evidence"`
}

// ReviewVerdictClean: nothing material was found; the review releases the
// turn itself.
type ReviewVerdictClean struct{}

// ReviewVerdictFailed: the review could not reach a verdict. It never
// advances the baseline, so the same change is reviewed again next time.
type ReviewVerdictFailed struct {
	Reason string `json:"reason"`
}

func (ReviewVerdictFindings) isReviewVerdict() {}
func (ReviewVerdictClean) isReviewVerdict()    {}
func (ReviewVerdictFailed) isReviewVerdict()   {}

func (ReviewVerdictFindings) IsClean() bool { return false }
func (ReviewVerdictClean) IsClean() bool    { return true }
func (ReviewVerdictFailed) IsClean() bool   { return false }

func (v ReviewVerdictFindings) IsComplete() bool { return len(v.Evidence.ReviewerFailures) == 0 }
func (ReviewVerdictClean) IsComplete() bool      { return true }
func (ReviewVerdictFailed) IsComplete() bool     { return false }

func (v ReviewVerdictFindings) MarshalJSON() ([]byte, error) {
	type body ReviewVerdictFindings
	return agentTaggedJSON("verdict", "findings", body(v))
}

func (ReviewVerdictClean) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("verdict", "clean", nil)
}

func (v ReviewVerdictFailed) MarshalJSON() ([]byte, error) {
	type body ReviewVerdictFailed
	return agentTaggedJSON("verdict", "failed", body(v))
}

// UnmarshalReviewVerdict decodes a ReviewVerdict by its "verdict" tag.
func UnmarshalReviewVerdict(data []byte) (ReviewVerdict, error) {
	tag, err := agentTagOf(data, "verdict")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "findings":
		var v ReviewVerdictFindings
		if err := json.Unmarshal(data, &v); err != nil {
			return nil, err
		}
		if v.Evidence.Lanes == nil {
			v.Evidence.Lanes = []ReviewLaneEvidence{}
		}
		return v, nil
	case "clean":
		return ReviewVerdictClean{}, nil
	case "failed":
		return agentDecodeInto[ReviewVerdictFailed, ReviewVerdict](data)
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `findings`, `clean`, `failed`", tag)
}

// ReviewPassEvidence is legacy lane evidence retained so persisted pending
// forwards still decode. Every member defaults when absent.
type ReviewPassEvidence struct {
	// Lanes is specialist lane outcomes from older review records.
	Lanes []ReviewLaneEvidence `json:"lanes"`
	// ReviewerFailures is reviewers that did not complete, even when other
	// reviewers found issues.
	ReviewerFailures []ReviewerFailure `json:"reviewer_failures,omitempty"`
}

// MarshalJSON writes an absent lane list as [], as serde does for an empty Vec.
func (e ReviewPassEvidence) MarshalJSON() ([]byte, error) {
	type plain ReviewPassEvidence
	if e.Lanes == nil {
		e.Lanes = []ReviewLaneEvidence{}
	}
	return agentMarshalJSON(plain(e))
}

// ReviewerFailure is a reviewer whose absence keeps an otherwise useful
// findings report incomplete.
type ReviewerFailure struct {
	Label  string `json:"label"`
	Reason string `json:"reason"`
}

// ReviewLaneEvidence is a specialist lane outcome from a legacy review record.
type ReviewLaneEvidence struct {
	ID      string      `json:"id"`
	Outcome LaneOutcome `json:"outcome"`
}

func (e *ReviewLaneEvidence) UnmarshalJSON(data []byte) error {
	var raw struct {
		ID      *string         `json:"id"`
		Outcome json.RawMessage `json:"outcome"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	if raw.ID == nil {
		return fmt.Errorf("missing field `id`")
	}
	if raw.Outcome == nil {
		return fmt.Errorf("missing field `outcome`")
	}
	outcome, err := UnmarshalLaneOutcome(raw.Outcome)
	if err != nil {
		return err
	}
	*e = ReviewLaneEvidence{ID: *raw.ID, Outcome: outcome}
	return nil
}

// LaneOutcome is a legacy lane's terminal state. Internally tagged by
// "outcome" in snake_case.
type LaneOutcome interface {
	isLaneOutcome()
	Describe() string
}

type LaneOutcomeCompleted struct{}

type LaneOutcomeCancelled struct{}

type LaneOutcomeFailed struct {
	Reason string `json:"reason"`
}

func (LaneOutcomeCompleted) isLaneOutcome() {}
func (LaneOutcomeCancelled) isLaneOutcome() {}
func (LaneOutcomeFailed) isLaneOutcome()    {}

func (LaneOutcomeCompleted) Describe() string { return "completed" }
func (LaneOutcomeCancelled) Describe() string { return "cancelled" }
func (o LaneOutcomeFailed) Describe() string  { return "failed: " + o.Reason }

func (LaneOutcomeCompleted) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("outcome", "completed", nil)
}

func (LaneOutcomeCancelled) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("outcome", "cancelled", nil)
}

func (o LaneOutcomeFailed) MarshalJSON() ([]byte, error) {
	type body LaneOutcomeFailed
	return agentTaggedJSON("outcome", "failed", body(o))
}

// UnmarshalLaneOutcome decodes a LaneOutcome by its "outcome" tag.
func UnmarshalLaneOutcome(data []byte) (LaneOutcome, error) {
	tag, err := agentTagOf(data, "outcome")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "completed":
		return LaneOutcomeCompleted{}, nil
	case "cancelled":
		return LaneOutcomeCancelled{}, nil
	case "failed":
		return agentDecodeInto[LaneOutcomeFailed, LaneOutcome](data)
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `completed`, `cancelled`, `failed`", tag)
}
