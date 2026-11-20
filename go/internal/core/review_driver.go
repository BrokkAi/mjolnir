// Port of mj-core/src/review/driver.rs.

// Shared turn-review requests, status and recovery data.

package core

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// ReviewRequest is what the driver needs the caller to do next. Externally
// tagged: a unit variant is its bare name, a struct variant {"Name": {...}}.
type ReviewRequest interface {
	isReviewRequest()
}

// ReviewRequestCaptureDelta asks the worker to capture pushable commits.
type ReviewRequestCaptureDelta struct{}

// ReviewRequestStartReviewer registers a fresh reviewer child with this
// initial prompt.
type ReviewRequestStartReviewer struct {
	Role      string `json:"role"`
	CommandID string `json:"command_id"`
	Prompt    string `json:"prompt"`
}

// ReviewRequestStartBriefWriter registers the brief writer's child with this
// initial prompt. Its role is BriefWriterRole, not a reviewer slot, and no
// reviewer starts until it hands back or fails.
type ReviewRequestStartBriefWriter struct {
	CommandID string `json:"command_id"`
	Prompt    string `json:"prompt"`
}

// ReviewRequestCloseReviewer closes the reviewer child.
type ReviewRequestCloseReviewer struct {
	Role string `json:"role"`
}

// ReviewRequestCloseBriefWriter closes the brief writer's child.
type ReviewRequestCloseBriefWriter struct{}

// ReviewRequestNoteBriefSkipped tells the requester the reviewers started
// without a brief, and why.
type ReviewRequestNoteBriefSkipped struct {
	Reason string `json:"reason"`
}

// ReviewRequestClose: the review is over; close its pane.
type ReviewRequestClose struct{}

func (ReviewRequestCaptureDelta) isReviewRequest()     {}
func (ReviewRequestStartReviewer) isReviewRequest()    {}
func (ReviewRequestStartBriefWriter) isReviewRequest() {}
func (ReviewRequestCloseReviewer) isReviewRequest()    {}
func (ReviewRequestCloseBriefWriter) isReviewRequest() {}
func (ReviewRequestNoteBriefSkipped) isReviewRequest() {}
func (ReviewRequestClose) isReviewRequest()            {}

func (ReviewRequestCaptureDelta) MarshalJSON() ([]byte, error) {
	return []byte(`"CaptureDelta"`), nil
}

func (v ReviewRequestStartReviewer) MarshalJSON() ([]byte, error) {
	type body ReviewRequestStartReviewer
	return agentExternalJSON("StartReviewer", body(v))
}

func (v ReviewRequestStartBriefWriter) MarshalJSON() ([]byte, error) {
	type body ReviewRequestStartBriefWriter
	return agentExternalJSON("StartBriefWriter", body(v))
}

func (v ReviewRequestCloseReviewer) MarshalJSON() ([]byte, error) {
	type body ReviewRequestCloseReviewer
	return agentExternalJSON("CloseReviewer", body(v))
}

func (ReviewRequestCloseBriefWriter) MarshalJSON() ([]byte, error) {
	return []byte(`"CloseBriefWriter"`), nil
}

func (v ReviewRequestNoteBriefSkipped) MarshalJSON() ([]byte, error) {
	type body ReviewRequestNoteBriefSkipped
	return agentExternalJSON("NoteBriefSkipped", body(v))
}

func (ReviewRequestClose) MarshalJSON() ([]byte, error) {
	return []byte(`"Close"`), nil
}

// UnmarshalReviewRequest decodes an externally tagged ReviewRequest.
func UnmarshalReviewRequest(data []byte) (ReviewRequest, error) {
	name, body, err := agentExternalVariant(data)
	if err != nil {
		return nil, err
	}
	unit := func(v ReviewRequest) (ReviewRequest, error) {
		if body != nil && string(body) != "null" {
			return nil, fmt.Errorf("invalid type: map, expected unit variant ReviewRequest::%s", name)
		}
		return v, nil
	}
	switch name {
	case "CaptureDelta":
		return unit(ReviewRequestCaptureDelta{})
	case "StartReviewer":
		return agentDecodeInto[ReviewRequestStartReviewer, ReviewRequest](body)
	case "StartBriefWriter":
		return agentDecodeInto[ReviewRequestStartBriefWriter, ReviewRequest](body)
	case "CloseReviewer":
		return agentDecodeInto[ReviewRequestCloseReviewer, ReviewRequest](body)
	case "CloseBriefWriter":
		return unit(ReviewRequestCloseBriefWriter{})
	case "NoteBriefSkipped":
		return agentDecodeInto[ReviewRequestNoteBriefSkipped, ReviewRequest](body)
	case "Close":
		return unit(ReviewRequestClose{})
	}
	return nil, fmt.Errorf("unknown variant `%s`", name)
}

// RoleState is which stage of a review one role is in.
type RoleState string

const (
	RoleStatePending  RoleState = "pending"
	RoleStateRunning  RoleState = "running"
	RoleStateClean    RoleState = "clean"
	RoleStateFindings RoleState = "findings"
	RoleStateFailed   RoleState = "failed"
)

func (s RoleState) Label() string {
	switch s {
	case RoleStatePending:
		return "pending"
	case RoleStateRunning:
		return "running"
	case RoleStateClean:
		return "done"
	case RoleStateFindings:
		return "findings"
	}
	return "failed"
}

// RoleStatus is one reviewing agent's row in the review pane. It crosses the
// daemon's snapshot to every surface, so it serializes.
type RoleStatus struct {
	Role  string    `json:"role"`
	Label string    `json:"label"`
	State RoleState `json:"state"`
	// ChildSessionID: the host projects its authoritative registration onto
	// this row.
	ChildSessionID *string `json:"child_session_id,omitempty"`
}

// UnmarshalJSON refuses unknown members (serde deny_unknown_fields).
func (s *RoleStatus) UnmarshalJSON(data []byte) error {
	type plain RoleStatus
	return agentDecodeStrict(data, (*plain)(s))
}

// Resolution is how a review ended.
type Resolution string

const (
	// ResolutionCompleted: delivery completed. Keep the terminal tag older
	// clients understand.
	ResolutionCompleted Resolution = "dismissed"
	// ResolutionCancelled: the user stopped the review; its commits remain
	// unreviewed.
	ResolutionCancelled Resolution = "cancelled"
)

// TurnReviewPhase is where the review has got to. Internally tagged by
// "phase" in snake_case.
type TurnReviewPhase interface {
	isTurnReviewPhase()
}

// TurnReviewPhaseCapturingDelta: asking the worker for committed work.
type TurnReviewPhaseCapturingDelta struct{}

// TurnReviewPhaseLaunchingReviewer: staging and starting the first reviewing
// agent.
type TurnReviewPhaseLaunchingReviewer struct{}

// TurnReviewPhaseWritingBrief: a writer is reading the change to hand the
// reviewers a relation brief. No reviewer has started.
type TurnReviewPhaseWritingBrief struct{}

// TurnReviewPhaseRunning: one or more reviewing agents are working.
type TurnReviewPhaseRunning struct {
	Roles []RoleStatus `json:"roles"`
}

// TurnReviewPhaseVerdict: the verdict is being delivered to the agent. The
// verdict's members sit beside the phase tag.
type TurnReviewPhaseVerdict struct {
	Verdict ReviewVerdict
}

// TurnReviewPhaseResolved encodes as {"phase":"resolved","<resolution>":null},
// serde's internal tagging of a newtype holding a unit enum variant.
type TurnReviewPhaseResolved struct {
	Resolution Resolution
}

func (TurnReviewPhaseCapturingDelta) isTurnReviewPhase()    {}
func (TurnReviewPhaseLaunchingReviewer) isTurnReviewPhase() {}
func (TurnReviewPhaseWritingBrief) isTurnReviewPhase()      {}
func (TurnReviewPhaseRunning) isTurnReviewPhase()           {}
func (TurnReviewPhaseVerdict) isTurnReviewPhase()           {}
func (TurnReviewPhaseResolved) isTurnReviewPhase()          {}

func (TurnReviewPhaseCapturingDelta) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("phase", "capturing_delta", nil)
}

func (TurnReviewPhaseLaunchingReviewer) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("phase", "launching_reviewer", nil)
}

func (TurnReviewPhaseWritingBrief) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("phase", "writing_brief", nil)
}

func (v TurnReviewPhaseRunning) MarshalJSON() ([]byte, error) {
	type body TurnReviewPhaseRunning
	if v.Roles == nil {
		v.Roles = []RoleStatus{}
	}
	return agentTaggedJSON("phase", "running", body(v))
}

func (v TurnReviewPhaseVerdict) MarshalJSON() ([]byte, error) {
	inner, err := agentMarshalJSON(v.Verdict)
	if err != nil {
		return nil, err
	}
	return agentTaggedJSON("phase", "verdict", json.RawMessage(inner))
}

func (v TurnReviewPhaseResolved) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("phase", "resolved", map[string]any{string(v.Resolution): nil})
}

// UnmarshalTurnReviewPhase decodes a TurnReviewPhase by its "phase" tag.
func UnmarshalTurnReviewPhase(data []byte) (TurnReviewPhase, error) {
	tag, err := agentTagOf(data, "phase")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "capturing_delta":
		return TurnReviewPhaseCapturingDelta{}, nil
	case "launching_reviewer":
		return TurnReviewPhaseLaunchingReviewer{}, nil
	case "writing_brief":
		return TurnReviewPhaseWritingBrief{}, nil
	case "running":
		return agentDecodeInto[TurnReviewPhaseRunning, TurnReviewPhase](data)
	case "verdict":
		verdict, err := UnmarshalReviewVerdict(data)
		if err != nil {
			return nil, err
		}
		return TurnReviewPhaseVerdict{Verdict: verdict}, nil
	case "resolved":
		var members map[string]json.RawMessage
		if err := json.Unmarshal(data, &members); err != nil {
			return nil, err
		}
		delete(members, "phase")
		if len(members) != 1 {
			return nil, fmt.Errorf("expected a single resolution variant")
		}
		for name, value := range members {
			resolution := Resolution(name)
			if resolution != ResolutionCompleted && resolution != ResolutionCancelled {
				return nil, fmt.Errorf("unknown variant `%s`, expected `dismissed` or `cancelled`", name)
			}
			if string(value) != "null" {
				return nil, fmt.Errorf("invalid type, expected unit variant Resolution::%s", name)
			}
			return TurnReviewPhaseResolved{Resolution: resolution}, nil
		}
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// BriefWriterRole is the role key of the brief writer's child. It is a
// registration of the review group but never a reviewer slot, so it has no
// verdict of its own.
const BriefWriterRole = "brief-writer"

// BriefWriterCommandID is the driver-local key for the brief stage. The
// daemon owns the child's startup prompt ID separately, through its
// registration request key.
func BriefWriterCommandID(reviewID string) string {
	return CommandIDPrefix + reviewID + "-" + BriefWriterRole
}

// ReviewerRole is the legacy sole-reviewer role, retained for old review
// records.
const ReviewerRole = "reviewer"

// CommandIDPrefix is how every command a turn review sends a reviewing role
// begins, so a reviewer's running prompt says which kind of review it
// belongs to.
const CommandIDPrefix = "turn-review-"

// TurnReviewSeed is everything about the reviewed turn that is known before
// the capture lands.
type TurnReviewSeed struct {
	// Task is the latest real user prompt; earlier requirements remain in
	// the history.
	Task string `json:"task"`
	// UserMessages is all real user messages in chronological order,
	// excluding harness notes.
	UserMessages []UserMessage `json:"user_messages"`
	// Declined is findings the author declined to fix, for the reviewer
	// prompt. An older snapshot has none.
	Declined []ReviewOverride `json:"declined"`
	// Brief is relations a brief writer computed for the reviewers, rendered
	// into the reviewer prompt. An older snapshot has none, and none renders
	// nothing.
	Brief *string `json:"brief"`
}

// MarshalJSON keeps old snapshot readers working while the runtime carries
// no turn state: it writes empty `baselines` and a zero `through_ordinal`.
func (s TurnReviewSeed) MarshalJSON() ([]byte, error) {
	out := struct {
		Task           string            `json:"task"`
		UserMessages   []UserMessage     `json:"user_messages"`
		Declined       []ReviewOverride  `json:"declined"`
		Baselines      map[string]string `json:"baselines"`
		ThroughOrdinal uint64            `json:"through_ordinal"`
		Brief          *string           `json:"brief,omitempty"`
	}{s.Task, s.UserMessages, s.Declined, map[string]string{}, 0, s.Brief}
	if out.UserMessages == nil {
		out.UserMessages = []UserMessage{}
	}
	if out.Declined == nil {
		out.Declined = []ReviewOverride{}
	}
	return agentMarshalJSON(out)
}

// ReviewerOutcomeSlot is one frozen reviewer slot. Its verdict is the only
// terminal-state fact.
type ReviewerOutcomeSlot struct {
	Role      string        `json:"role"`
	CommandID string        `json:"command_id"`
	Outcome   ReviewVerdict `json:"outcome"`
}

func (s *ReviewerOutcomeSlot) UnmarshalJSON(data []byte) error {
	var raw struct {
		Role      *string         `json:"role"`
		CommandID *string         `json:"command_id"`
		Outcome   json.RawMessage `json:"outcome"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	if raw.Role == nil {
		return fmt.Errorf("missing field `role`")
	}
	if raw.CommandID == nil {
		return fmt.Errorf("missing field `command_id`")
	}
	slot := ReviewerOutcomeSlot{Role: *raw.Role, CommandID: *raw.CommandID}
	if raw.Outcome != nil && string(raw.Outcome) != "null" {
		outcome, err := UnmarshalReviewVerdict(raw.Outcome)
		if err != nil {
			return err
		}
		slot.Outcome = outcome
	}
	*s = slot
	return nil
}

// ReviewDriverSnapshot is durable pure-driver state. Child registration and
// live identity remain host-owned.
type ReviewDriverSnapshot struct {
	ReviewID    string                `json:"review_id"`
	Seed        TurnReviewSeed        `json:"seed"`
	Phase       TurnReviewPhase       `json:"phase"`
	LastVerdict ReviewVerdict         `json:"last_verdict"`
	Deltas      []RepoDelta           `json:"deltas"`
	Roles       []ReviewerOutcomeSlot `json:"roles"`
	Sequence    uint64                `json:"sequence"`
	// BriefWriter is the brief writer's row state: running while it writes,
	// then clean once its brief is in the prompts or failed when the
	// reviewers went without. nil for a review with no writer.
	BriefWriter *RoleState `json:"brief_writer"`
	// ReplayRequests is the exact requests emitted by the current stage,
	// before any side effect.
	ReplayRequests []ReviewRequest `json:"replay_requests"`
}

// UnmarshalJSON decodes the polymorphic members and reads ReplayRequests
// through deserializeReplayRequests.
func (s *ReviewDriverSnapshot) UnmarshalJSON(data []byte) error {
	var raw struct {
		ReviewID       *string                `json:"review_id"`
		Seed           *TurnReviewSeed        `json:"seed"`
		Phase          json.RawMessage        `json:"phase"`
		LastVerdict    json.RawMessage        `json:"last_verdict"`
		Deltas         *[]RepoDelta           `json:"deltas"`
		Roles          *[]ReviewerOutcomeSlot `json:"roles"`
		Sequence       *uint64                `json:"sequence"`
		BriefWriter    *RoleState             `json:"brief_writer"`
		ReplayRequests json.RawMessage        `json:"replay_requests"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	switch {
	case raw.ReviewID == nil:
		return fmt.Errorf("missing field `review_id`")
	case raw.Seed == nil:
		return fmt.Errorf("missing field `seed`")
	case raw.Phase == nil:
		return fmt.Errorf("missing field `phase`")
	case raw.Deltas == nil:
		return fmt.Errorf("missing field `deltas`")
	case raw.Roles == nil:
		return fmt.Errorf("missing field `roles`")
	case raw.Sequence == nil:
		return fmt.Errorf("missing field `sequence`")
	}
	phase, err := UnmarshalTurnReviewPhase(raw.Phase)
	if err != nil {
		return err
	}
	snapshot := ReviewDriverSnapshot{
		ReviewID:       *raw.ReviewID,
		Seed:           *raw.Seed,
		Phase:          phase,
		Deltas:         *raw.Deltas,
		Roles:          *raw.Roles,
		Sequence:       *raw.Sequence,
		BriefWriter:    raw.BriefWriter,
		ReplayRequests: []ReviewRequest{},
	}
	if raw.LastVerdict != nil && string(raw.LastVerdict) != "null" {
		if snapshot.LastVerdict, err = UnmarshalReviewVerdict(raw.LastVerdict); err != nil {
			return err
		}
	}
	if raw.ReplayRequests != nil {
		if snapshot.ReplayRequests, err = deserializeReplayRequests(raw.ReplayRequests); err != nil {
			return err
		}
	}
	*s = snapshot
	return nil
}

// deserializeReplayRequests keeps the requests this build knows, reading an
// old `CaptureDelta` with baselines as the bare request.
func deserializeReplayRequests(data []byte) ([]ReviewRequest, error) {
	var values []json.RawMessage
	if err := json.Unmarshal(data, &values); err != nil {
		return nil, err
	}
	requests := []ReviewRequest{}
	for _, value := range values {
		var kind string
		if err := json.Unmarshal(value, &kind); err != nil {
			kind, _ = agentFirstKey(value)
		}
		switch kind {
		case "CaptureDelta", "StartReviewer", "StartBriefWriter", "CloseReviewer",
			"CloseBriefWriter", "NoteBriefSkipped", "Close":
		default:
			continue
		}
		var members map[string]json.RawMessage
		if json.Unmarshal(value, &members) == nil {
			if _, ok := members["CaptureDelta"]; ok {
				requests = append(requests, ReviewRequestCaptureDelta{})
				continue
			}
		}
		request, err := UnmarshalReviewRequest(value)
		if err != nil {
			return nil, err
		}
		requests = append(requests, request)
	}
	return requests, nil
}

// agentFirstKey is the first member name of a JSON object, in document
// order (serde_json here preserves order).
func agentFirstKey(data []byte) (string, bool) {
	decoder := json.NewDecoder(bytes.NewReader(data))
	if token, err := decoder.Token(); err != nil || token != json.Delim('{') {
		return "", false
	}
	token, err := decoder.Token()
	if err != nil {
		return "", false
	}
	key, ok := token.(string)
	return key, ok
}

// MarshalJSON writes the snapshot in the shape older readers expect: a
// fixed `provenance`, and `CaptureDelta` replay requests with empty
// baselines.
func (s ReviewDriverSnapshot) MarshalJSON() ([]byte, error) {
	requests := make([]json.RawMessage, 0, len(s.ReplayRequests))
	for _, request := range s.ReplayRequests {
		if _, ok := request.(ReviewRequestCaptureDelta); ok {
			requests = append(requests, json.RawMessage(`{"CaptureDelta":{"baselines":{}}}`))
			continue
		}
		encoded, err := agentMarshalJSON(request)
		if err != nil {
			return nil, err
		}
		requests = append(requests, encoded)
	}
	out := struct {
		ReviewID       string                `json:"review_id"`
		Seed           TurnReviewSeed        `json:"seed"`
		Phase          TurnReviewPhase       `json:"phase"`
		LastVerdict    ReviewVerdict         `json:"last_verdict"`
		Deltas         []RepoDelta           `json:"deltas"`
		Roles          []ReviewerOutcomeSlot `json:"roles"`
		Provenance     string                `json:"provenance"`
		Sequence       uint64                `json:"sequence"`
		ReplayRequests []json.RawMessage     `json:"replay_requests"`
		BriefWriter    *RoleState            `json:"brief_writer,omitempty"`
	}{s.ReviewID, s.Seed, s.Phase, s.LastVerdict, s.Deltas, s.Roles,
		"independent_reviewers", s.Sequence, requests, s.BriefWriter}
	if out.Deltas == nil {
		out.Deltas = []RepoDelta{}
	}
	if out.Roles == nil {
		out.Roles = []ReviewerOutcomeSlot{}
	}
	return agentMarshalJSON(out)
}
