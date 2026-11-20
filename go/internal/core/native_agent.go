// Port of mj-core/src/native_agent.rs.

// Harness-owned children share their owner's worker, but not its transcript.

package core

import (
	"encoding/json"
	"fmt"
	"strings"

	acpschema "github.com/BrokkAi/acp-go/schema"
)

type NativeAgentCapabilities struct {
	Cancel bool `json:"cancel"`
	Close  bool `json:"close"`
}

type NativeAgentState string

const (
	NativeAgentStateRunning      NativeAgentState = "running"
	NativeAgentStateCompleted    NativeAgentState = "completed"
	NativeAgentStateFailed       NativeAgentState = "failed"
	NativeAgentStateCancelled    NativeAgentState = "cancelled"
	NativeAgentStateDisconnected NativeAgentState = "disconnected"
)

// NativeAgentAvailability: availability for another turn is independent of
// the last turn's outcome. The zero value is Unknown, the default.
type NativeAgentAvailability int

const (
	NativeAgentAvailabilityUnknown NativeAgentAvailability = iota
	NativeAgentAvailabilityAvailable
	NativeAgentAvailabilityUnavailable
)

func (a NativeAgentAvailability) MarshalJSON() ([]byte, error) {
	switch a {
	case NativeAgentAvailabilityAvailable:
		return []byte(`"available"`), nil
	case NativeAgentAvailabilityUnknown:
		return []byte(`"unknown"`), nil
	case NativeAgentAvailabilityUnavailable:
		return []byte(`"unavailable"`), nil
	}
	return nil, fmt.Errorf("invalid native agent availability %d", int(a))
}

func (a *NativeAgentAvailability) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch text {
	case "available":
		*a = NativeAgentAvailabilityAvailable
	case "unknown":
		*a = NativeAgentAvailabilityUnknown
	case "unavailable":
		*a = NativeAgentAvailabilityUnavailable
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `available`, `unknown`, `unavailable`", text)
	}
	return nil
}

func (a NativeAgentAvailability) Label() string {
	switch a {
	case NativeAgentAvailabilityAvailable:
		return "reusable"
	case NativeAgentAvailabilityUnknown:
		return "availability unknown"
	}
	return "unavailable"
}

// NativeAgentAvailabilityReport is a negotiated, read-only inventory entry.
// IDs are opaque provider identities.
type NativeAgentAvailabilityReport struct {
	SessionID    string                  `json:"session_id"`
	StableID     *string                 `json:"stable_id"`
	Availability NativeAgentAvailability `json:"availability"`
	State        *NativeAgentState       `json:"state"`
	Reason       *string                 `json:"reason"`
}

func (r *NativeAgentAvailabilityReport) UnmarshalJSON(data []byte) error {
	type plain NativeAgentAvailabilityReport
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	var required struct {
		SessionID    json.RawMessage `json:"session_id"`
		Availability json.RawMessage `json:"availability"`
	}
	if err := json.Unmarshal(data, &required); err != nil {
		return err
	}
	if required.SessionID == nil {
		return fmt.Errorf("missing field `session_id`")
	}
	if required.Availability == nil {
		return fmt.Errorf("missing field `availability`")
	}
	*r = NativeAgentAvailabilityReport(value)
	return nil
}

type NativeAgent struct {
	Availability       NativeAgentAvailability `json:"availability"`
	AvailabilityReason *string                 `json:"availability_reason,omitempty"`
	StableID           *string                 `json:"stable_id,omitempty"`
	OwnerSessionID     string                  `json:"owner_session_id"`
	SessionID          string                  `json:"session_id"`
	ParentSessionID    *string                 `json:"parent_session_id"`
	Name               string                  `json:"name"`
	Task               string                  `json:"task"`
	Capabilities       NativeAgentCapabilities `json:"capabilities"`
	State              NativeAgentState        `json:"state"`
}

func (a *NativeAgent) InvalidateAvailability() {
	reason := "The current harness has not confirmed that this agent can resume"
	a.Availability = NativeAgentAvailabilityUnknown
	a.AvailabilityReason = &reason
	a.Capabilities = NativeAgentCapabilities{}
}

// FinishReplay: replayed work is historical until the current harness
// confirms activity.
func (a *NativeAgent) FinishReplay() {
	a.InvalidateAvailability()
	if a.State == NativeAgentStateRunning {
		a.State = NativeAgentStateDisconnected
	}
}

// ActivityLabel is what the agent is doing or how its work ended: "working",
// "completed", "failed", "interrupted" or "disconnected".
func (a *NativeAgent) ActivityLabel() string {
	switch a.State {
	case NativeAgentStateRunning:
		return "working"
	case NativeAgentStateCompleted:
		return "completed"
	case NativeAgentStateFailed:
		return "failed"
	case NativeAgentStateCancelled:
		return "interrupted"
	}
	return "disconnected"
}

func (a *NativeAgent) LifecycleLabel() string {
	return a.ActivityLabel() + " · " + a.Availability.Label()
}

func (a *NativeAgent) ApplyAvailability(reports []NativeAgentAvailabilityReport, complete bool) {
	for i := range reports {
		report := &reports[i]
		if report.SessionID != a.SessionID {
			continue
		}
		a.Availability = report.Availability
		if report.State != nil {
			a.State = *report.State
		}
		a.AvailabilityReason = nativeAgentCloneString(report.Reason)
		a.StableID = nativeAgentCloneString(report.StableID)
		return
	}
	if complete {
		reason := "Absent from the current harness inventory"
		a.Availability = NativeAgentAvailabilityUnavailable
		a.AvailabilityReason = &reason
	}
}

func nativeAgentCloneString(value *string) *string {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// ViewID is the presentation identity only; never a separately provisioned
// session.
func (a *NativeAgent) ViewID() string {
	return ViewID(a.OwnerSessionID, a.SessionID)
}

func (a *NativeAgent) ParentViewID() string {
	if a.ParentSessionID == nil {
		return a.OwnerSessionID
	}
	return ViewID(a.OwnerSessionID, *a.ParentSessionID)
}

// viewIDPrefix marks a native child's presentation id. Mjolnir session ids
// are hex, so no real session id starts with it.
const viewIDPrefix = "native:"

func ViewID(owner, child string) string {
	// Length-prefixing keeps opaque provider IDs collision-free.
	return OwnerViewIDPrefix(owner) + child
}

// OwnerViewIDPrefix is the start of every ViewID that owner owns.
func OwnerViewIDPrefix(owner string) string {
	return fmt.Sprintf("%s%d:%s", viewIDPrefix, len(owner), owner)
}

// IsViewID reports whether id is a native child's presentation id from
// ViewID.
func IsViewID(id string) bool {
	return strings.HasPrefix(id, viewIDPrefix)
}

// NativeAgentEvent is internally tagged by "kind" (snake_case variant
// names). Decode one with UnmarshalNativeAgentEvent.
type NativeAgentEvent interface {
	isNativeAgentEvent()
}

type NativeAgentEventAvailability struct {
	Reports  []NativeAgentAvailabilityReport `json:"reports"`
	Complete bool                            `json:"complete"`
}

type NativeAgentEventSpawned struct {
	SessionID       string                  `json:"session_id"`
	ParentSessionID *string                 `json:"parent_session_id"`
	Name            string                  `json:"name"`
	Task            string                  `json:"task"`
	Capabilities    NativeAgentCapabilities `json:"capabilities"`
}

type NativeAgentEventState struct {
	SessionID string           `json:"session_id"`
	State     NativeAgentState `json:"state"`
}

type NativeAgentEventUpdate struct {
	SessionID string                  `json:"session_id"`
	Update    acpschema.SessionUpdate `json:"update"`
}

type NativeAgentEventReplayBegin struct{}

type NativeAgentEventReplayCommit struct{}

type NativeAgentEventDisconnected struct{}

func (NativeAgentEventAvailability) isNativeAgentEvent() {}
func (NativeAgentEventSpawned) isNativeAgentEvent()      {}
func (NativeAgentEventState) isNativeAgentEvent()        {}
func (NativeAgentEventUpdate) isNativeAgentEvent()       {}
func (NativeAgentEventReplayBegin) isNativeAgentEvent()  {}
func (NativeAgentEventReplayCommit) isNativeAgentEvent() {}
func (NativeAgentEventDisconnected) isNativeAgentEvent() {}

// nativeAgentEventTagged writes fields (a struct value without its own
// MarshalJSON) as an object whose first key is "kind", as serde's internal
// tagging does.
func nativeAgentEventTagged(kind string, fields any) ([]byte, error) {
	tag, err := json.Marshal(kind)
	if err != nil {
		return nil, err
	}
	out := append([]byte(`{"kind":`), tag...)
	if fields == nil {
		return append(out, '}'), nil
	}
	body, err := json.Marshal(fields)
	if err != nil {
		return nil, err
	}
	if len(body) > 2 {
		out = append(out, ',')
		out = append(out, body[1:len(body)-1]...)
	}
	return append(out, '}'), nil
}

func (e NativeAgentEventAvailability) MarshalJSON() ([]byte, error) {
	type plain NativeAgentEventAvailability
	value := plain(e)
	if value.Reports == nil {
		value.Reports = []NativeAgentAvailabilityReport{}
	}
	return nativeAgentEventTagged("availability", value)
}

func (e NativeAgentEventSpawned) MarshalJSON() ([]byte, error) {
	type plain NativeAgentEventSpawned
	return nativeAgentEventTagged("spawned", plain(e))
}

func (e NativeAgentEventState) MarshalJSON() ([]byte, error) {
	type plain NativeAgentEventState
	return nativeAgentEventTagged("state", plain(e))
}

func (e NativeAgentEventUpdate) MarshalJSON() ([]byte, error) {
	type plain NativeAgentEventUpdate
	return nativeAgentEventTagged("update", plain(e))
}

func (NativeAgentEventReplayBegin) MarshalJSON() ([]byte, error) {
	return nativeAgentEventTagged("replay_begin", nil)
}

func (NativeAgentEventReplayCommit) MarshalJSON() ([]byte, error) {
	return nativeAgentEventTagged("replay_commit", nil)
}

func (NativeAgentEventDisconnected) MarshalJSON() ([]byte, error) {
	return nativeAgentEventTagged("disconnected", nil)
}

// UnmarshalNativeAgentEvent decodes a "kind"-tagged NativeAgentEvent.
func UnmarshalNativeAgentEvent(data []byte) (NativeAgentEvent, error) {
	var tag struct {
		Kind *string `json:"kind"`
	}
	if err := json.Unmarshal(data, &tag); err != nil {
		return nil, err
	}
	if tag.Kind == nil {
		return nil, fmt.Errorf("missing field `kind`")
	}
	switch *tag.Kind {
	case "availability":
		var event NativeAgentEventAvailability
		err := json.Unmarshal(data, &event)
		return event, err
	case "spawned":
		var event NativeAgentEventSpawned
		err := json.Unmarshal(data, &event)
		return event, err
	case "state":
		var event NativeAgentEventState
		err := json.Unmarshal(data, &event)
		return event, err
	case "update":
		var event NativeAgentEventUpdate
		err := json.Unmarshal(data, &event)
		return event, err
	case "replay_begin":
		return NativeAgentEventReplayBegin{}, nil
	case "replay_commit":
		return NativeAgentEventReplayCommit{}, nil
	case "disconnected":
		return NativeAgentEventDisconnected{}, nil
	}
	return nil, fmt.Errorf("unknown variant `%s` of native agent event", *tag.Kind)
}

type NativeAgentView struct {
	GenerationOrdinal uint64              `json:"generation_ordinal"`
	Agent             NativeAgent         `json:"agent"`
	Projection        MaterializedSession `json:"projection"`
	PendingInputTimes map[string]int64    `json:"pending_input_times,omitempty"`
}

// ApplyActivity applies a native activity event and any change to the
// projected forms. A nil pendingElicitations leaves the projected forms as
// they are; a non-nil (even empty) one replaces them.
func (v *NativeAgentView) ApplyActivity(recordedAtMs int64, pendingElicitations *[]ElicitationRequest) {
	// Older projections omitted raise times. Capture the best recorded
	// time before applying the next activity event.
	if len(v.PendingInputTimes) == 0 {
		previousTime := int64(0)
		if v.Projection.LastActivityAtMs != nil {
			previousTime = *v.Projection.LastActivityAtMs
		}
		for _, request := range v.Projection.PendingElicitations {
			if v.PendingInputTimes == nil {
				v.PendingInputTimes = map[string]int64{}
			}
			v.PendingInputTimes[request.ID] = previousTime
		}
	}
	if pendingElicitations != nil {
		times := map[string]int64{}
		for _, request := range *pendingElicitations {
			raisedAtMs, ok := v.PendingInputTimes[request.ID]
			if !ok {
				raisedAtMs = recordedAtMs
			}
			times[request.ID] = raisedAtMs
		}
		v.PendingInputTimes = times
		v.Projection.PendingElicitations = *pendingElicitations
	}
	v.Projection.LastActivityAtMs = &recordedAtMs
}

// NativeAgentSummary is the transcript-free identity published by the
// controller runtime feed.
type NativeAgentSummary struct {
	GenerationOrdinal uint64      `json:"generation_ordinal"`
	Agent             NativeAgent `json:"agent"`
	ProjectionOrdinal uint64      `json:"projection_ordinal"`
	ProjectionDigest  string      `json:"projection_digest"`
	// PendingInput is a native operational fact needed by the daemon
	// attention projection.
	PendingInput          bool    `json:"pending_input"`
	PendingInputMessage   *string `json:"pending_input_message,omitempty"`
	PendingInputRequestID *string `json:"pending_input_request_id,omitempty"`
	// PendingInputRaisedAtMs is when the current request first appeared in
	// the native projection.
	PendingInputRaisedAtMs *int64 `json:"pending_input_raised_at_ms,omitempty"`
}

func NativeAgentSummaryOf(view *NativeAgentView) NativeAgentSummary {
	summary := NativeAgentSummary{
		GenerationOrdinal: view.GenerationOrdinal,
		Agent:             view.Agent.clone(),
		ProjectionOrdinal: view.Projection.AppliedEventOrdinal,
		ProjectionDigest:  view.Projection.AppliedEventDigest,
	}
	pending := view.Projection.PendingElicitations
	if len(pending) == 0 {
		return summary
	}
	request := pending[len(pending)-1]
	summary.PendingInput = true
	message, id := request.Message, request.ID
	summary.PendingInputMessage = &message
	summary.PendingInputRequestID = &id
	if len(view.PendingInputTimes) == 0 {
		if view.Projection.LastActivityAtMs != nil {
			raised := *view.Projection.LastActivityAtMs
			summary.PendingInputRaisedAtMs = &raised
		}
	} else if raised, ok := view.PendingInputTimes[request.ID]; ok {
		summary.PendingInputRaisedAtMs = &raised
	}
	return summary
}

// clone copies the agent with its own optional strings.
func (a NativeAgent) clone() NativeAgent {
	a.AvailabilityReason = nativeAgentCloneString(a.AvailabilityReason)
	a.StableID = nativeAgentCloneString(a.StableID)
	a.ParentSessionID = nativeAgentCloneString(a.ParentSessionID)
	return a
}

// equal is Rust's derived PartialEq: optional strings compare by value.
func (a *NativeAgent) equal(other *NativeAgent) bool {
	same := func(left, right *string) bool {
		return (left == nil) == (right == nil) && (left == nil || *left == *right)
	}
	return a.Availability == other.Availability &&
		same(a.AvailabilityReason, other.AvailabilityReason) &&
		same(a.StableID, other.StableID) &&
		a.OwnerSessionID == other.OwnerSessionID &&
		a.SessionID == other.SessionID &&
		same(a.ParentSessionID, other.ParentSessionID) &&
		a.Name == other.Name &&
		a.Task == other.Task &&
		a.Capabilities == other.Capabilities &&
		a.State == other.State
}

func (s *NativeAgentSummary) IsSatisfiedBy(view *NativeAgentView) bool {
	return s.GenerationOrdinal == view.GenerationOrdinal &&
		(view.Projection.AppliedEventOrdinal > s.ProjectionOrdinal ||
			(view.Projection.AppliedEventOrdinal == s.ProjectionOrdinal &&
				view.Projection.AppliedEventDigest == s.ProjectionDigest &&
				view.Agent.equal(&s.Agent)))
}

type NativeAgentHistoryPage struct {
	GenerationOrdinal uint64            `json:"generation_ordinal"`
	Items             []*TranscriptItem `json:"items"`
	HasMore           bool              `json:"has_more"`
}
