// Port of mj-core/src/relay/types.rs.
//
// Worker-facing types unrelated to the durable relay: legacy pre-relay
// event/snapshot shapes still used to import old histories into the
// controller-owned projection, plus small shared value types like Attachment
// and QueuedPrompt.

package core

import (
	"encoding/json"
	"fmt"
	"reflect"
)

type Attachment struct {
	Name      string `json:"name"`
	MediaType string `json:"media_type"`
	// A target-local path or opaque adapter reference. File contents do not
	// travel in control messages.
	Reference string `json:"reference"`
}

type WorkerPhase string

const (
	WorkerPhaseIdle    WorkerPhase = "idle"
	WorkerPhaseRunning WorkerPhase = "running"
	WorkerPhaseClosing WorkerPhase = "closing"
	WorkerPhaseClosed  WorkerPhase = "closed"
)

type ActivePrompt struct {
	RequestID   string       `json:"request_id"`
	Text        string       `json:"text"`
	Attachments []Attachment `json:"attachments"`
}

// QueuedPrompt refuses unknown fields; Kind is omitted when it is a prompt.
type QueuedPrompt struct {
	ID          string            `json:"id"`
	Kind        QueuedCommandKind `json:"kind"`
	Text        string            `json:"text"`
	Attachments []Attachment      `json:"attachments"`
	CreatedAtMs int64             `json:"created_at_ms"`
}

// MarshalJSON omits Kind for a prompt (serde skip_serializing_if is_prompt).
func (q QueuedPrompt) MarshalJSON() ([]byte, error) {
	var kind *QueuedCommandKind
	if q.Kind != nil && !q.Kind.IsPrompt() {
		kind = &q.Kind
	}
	return relayMarshalJSON(struct {
		ID          string             `json:"id"`
		Kind        *QueuedCommandKind `json:"kind,omitempty"`
		Text        string             `json:"text"`
		Attachments []Attachment       `json:"attachments"`
		CreatedAtMs int64              `json:"created_at_ms"`
	}{q.ID, kind, q.Text, q.Attachments, q.CreatedAtMs})
}

// UnmarshalJSON refuses unknown fields; a missing kind is a prompt.
func (q *QueuedPrompt) UnmarshalJSON(data []byte) error {
	type plain QueuedPrompt
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return fmt.Errorf("queued prompt: %w", err)
	}
	// Kind is a sealed interface, so it is read raw and decoded by
	// unmarshalQueuedCommandKind.
	var wire struct {
		plain
		Kind json.RawMessage `json:"kind"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*q = QueuedPrompt(wire.plain)
	q.Kind = QueuedCommandKindPrompt{}
	if wire.Kind != nil {
		kind, err := unmarshalQueuedCommandKind(wire.Kind)
		if err != nil {
			return err
		}
		q.Kind = kind
	}
	return nil
}

// WorkerSnapshot is the minimal snapshot shape retained for importing
// pre-relay event histories into the controller-owned materialized
// projection. It is not a relay wire message and is never persisted by
// DurableRelay.
type WorkerSnapshot struct {
	SessionID         string                     `json:"session_id"`
	Phase             WorkerPhase                `json:"phase"`
	LatestSeq         uint64                     `json:"latest_seq"`
	LastCheckpointSeq *uint64                    `json:"last_checkpoint_seq"`
	ActivePrompt      *ActivePrompt              `json:"active_prompt"`
	Config            map[string]json.RawMessage `json:"config"`
	QueuedPrompts     []QueuedPrompt             `json:"queued_prompts,omitempty"`
}

func WorkerSnapshotSummary(sessionID string, phase WorkerPhase, latestSeq uint64) WorkerSnapshot {
	return WorkerSnapshot{
		SessionID: sessionID,
		Phase:     phase,
		LatestSeq: latestSeq,
		Config:    map[string]json.RawMessage{},
	}
}

type SequencedEvent struct {
	Seq uint64 `json:"seq"`
	// UTC receive/accept time recorded by the durable worker. Legacy and
	// imported event streams may omit it.
	RecordedAtMs *int64 `json:"recorded_at_ms,omitempty"`
	// Present for controller mutations. Persisting the id beside the event
	// closes the crash window between appending the event and snapshotting
	// the idempotency result.
	RequestID *string     `json:"request_id,omitempty"`
	Event     WorkerEvent `json:"event"`
}

// UnmarshalJSON decodes Event through its variant table.
func (e *SequencedEvent) UnmarshalJSON(data []byte) error {
	type plain SequencedEvent
	aux := struct {
		*plain
		Event json.RawMessage `json:"event"`
	}{plain: (*plain)(e)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if aux.Event == nil {
		return fmt.Errorf("missing field %q", "event")
	}
	event, err := UnmarshalWorkerEventJSON(aux.Event)
	if err != nil {
		return err
	}
	e.Event = event
	return nil
}

// WorkerEvent is the event shape used only to decode/import histories
// produced before relay-v1.
type WorkerEvent interface {
	json.Marshaler
	isWorkerEvent()
}

type WorkerEventPromptAccepted struct {
	RequestID   string       `json:"request_id"`
	Text        string       `json:"text"`
	Attachments []Attachment `json:"attachments"`
}

func (WorkerEventPromptAccepted) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventPromptAccepted.
func (v WorkerEventPromptAccepted) MarshalJSON() ([]byte, error) {
	type plain WorkerEventPromptAccepted
	return relayAdjacentJSON("type", "data", "prompt_accepted", plain(v))
}

type WorkerEventQueuedPromptAdded struct {
	Prompt QueuedPrompt `json:"prompt"`
}

func (WorkerEventQueuedPromptAdded) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventQueuedPromptAdded.
func (v WorkerEventQueuedPromptAdded) MarshalJSON() ([]byte, error) {
	type plain WorkerEventQueuedPromptAdded
	return relayAdjacentJSON("type", "data", "queued_prompt_added", plain(v))
}

type WorkerEventQueuedPromptRemoved struct {
	QueueID string `json:"queue_id"`
}

func (WorkerEventQueuedPromptRemoved) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventQueuedPromptRemoved.
func (v WorkerEventQueuedPromptRemoved) MarshalJSON() ([]byte, error) {
	type plain WorkerEventQueuedPromptRemoved
	return relayAdjacentJSON("type", "data", "queued_prompt_removed", plain(v))
}

type WorkerEventQueuedPromptPromoted struct {
	Prompt    QueuedPrompt `json:"prompt"`
	RequestID string       `json:"request_id"`
}

func (WorkerEventQueuedPromptPromoted) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventQueuedPromptPromoted.
func (v WorkerEventQueuedPromptPromoted) MarshalJSON() ([]byte, error) {
	type plain WorkerEventQueuedPromptPromoted
	return relayAdjacentJSON("type", "data", "queued_prompt_promoted", plain(v))
}

type WorkerEventQueuedPromptsCleared struct{}

func (WorkerEventQueuedPromptsCleared) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventQueuedPromptsCleared.
func (v WorkerEventQueuedPromptsCleared) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "queued_prompts_cleared", nil)
}

type WorkerEventTurnCompleted struct{}

func (WorkerEventTurnCompleted) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventTurnCompleted.
func (v WorkerEventTurnCompleted) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "turn_completed", nil)
}

type WorkerEventCancelled struct{}

func (WorkerEventCancelled) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventCancelled.
func (v WorkerEventCancelled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "cancelled", nil)
}

type WorkerEventConfigChanged struct {
	Key   string          `json:"key"`
	Value json.RawMessage `json:"value"`
}

func (WorkerEventConfigChanged) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventConfigChanged.
func (v WorkerEventConfigChanged) MarshalJSON() ([]byte, error) {
	type plain WorkerEventConfigChanged
	return relayAdjacentJSON("type", "data", "config_changed", plain(v))
}

type WorkerEventCheckpointed struct {
	Reason    *string `json:"reason"`
	Quiescent bool    `json:"quiescent,omitempty"`
}

func (WorkerEventCheckpointed) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventCheckpointed.
func (v WorkerEventCheckpointed) MarshalJSON() ([]byte, error) {
	type plain WorkerEventCheckpointed
	return relayAdjacentJSON("type", "data", "checkpointed", plain(v))
}

type WorkerEventClosing struct{}

func (WorkerEventClosing) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventClosing.
func (v WorkerEventClosing) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "closing", nil)
}

type WorkerEventClosed struct{}

func (WorkerEventClosed) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventClosed.
func (v WorkerEventClosed) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "closed", nil)
}

type WorkerEventAdapter struct {
	Kind    string          `json:"kind"`
	Payload json.RawMessage `json:"payload"`
}

func (WorkerEventAdapter) isWorkerEvent() {}

// MarshalJSON writes the serde-tagged form of WorkerEventAdapter.
func (v WorkerEventAdapter) MarshalJSON() ([]byte, error) {
	type plain WorkerEventAdapter
	return relayAdjacentJSON("type", "data", "adapter", plain(v))
}

// workerEventVariants decodes each WorkerEvent variant by its wire name.
var workerEventVariants = map[string]func(json.RawMessage) (WorkerEvent, error){
	"prompt_accepted":        relayVariant[WorkerEventPromptAccepted, WorkerEvent](false),
	"queued_prompt_added":    relayVariant[WorkerEventQueuedPromptAdded, WorkerEvent](false),
	"queued_prompt_removed":  relayVariant[WorkerEventQueuedPromptRemoved, WorkerEvent](false),
	"queued_prompt_promoted": relayVariant[WorkerEventQueuedPromptPromoted, WorkerEvent](false),
	"queued_prompts_cleared": relayVariant[WorkerEventQueuedPromptsCleared, WorkerEvent](false),
	"turn_completed":         relayVariant[WorkerEventTurnCompleted, WorkerEvent](false),
	"cancelled":              relayVariant[WorkerEventCancelled, WorkerEvent](false),
	"config_changed":         relayVariant[WorkerEventConfigChanged, WorkerEvent](false),
	"checkpointed":           relayVariant[WorkerEventCheckpointed, WorkerEvent](false),
	"closing":                relayVariant[WorkerEventClosing, WorkerEvent](false),
	"closed":                 relayVariant[WorkerEventClosed, WorkerEvent](false),
	"adapter":                relayVariant[WorkerEventAdapter, WorkerEvent](false),
}

// UnmarshalWorkerEventJSON decodes the serde-tagged form of WorkerEvent.
func UnmarshalWorkerEventJSON(data []byte) (WorkerEvent, error) {
	name, body, err := relaySplitAdjacent(data, "type", "data", false)
	if err != nil {
		return nil, fmt.Errorf("decode WorkerEvent: %w", err)
	}
	decode, ok := workerEventVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode WorkerEvent: unknown variant %q", name)
	}
	return decode(body)
}
