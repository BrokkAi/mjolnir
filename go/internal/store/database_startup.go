// Port of mj-store/src/database/startup.rs.

package store

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type StartupDelivery struct {
	SessionID       string
	CommandID       string
	StepJSON        string
	Phase           string
	GroupID         *string
	AcceptedOrdinal *uint64
	Error           *string
}

// StartupGroupStatus is what status readers need from a session's latest
// startup group, computed once when the group is loaded, so the daemon keeps
// no per-step strings.
type StartupGroupStatus struct {
	GroupID *string
	// Failure is the first failed or rejecting step's error (nil inside when
	// that step recorded none); nil when no step failed.
	Failure **string
	// Pending is whether any step is still pending, delivering or accepted.
	Pending bool
	// LastDone is set when the last step is done: whether it was an API
	// prompt (or why its payload could not be read) and its acceptance
	// ordinal. Parse failures stay visible at the same decision point as
	// before.
	LastDone *StartupLastDone
}

// StartupLastDone is StartupGroupStatus's last finished step (Rust's
// (Result<bool, String>, Option<u64>)).
type StartupLastDone struct {
	// APIPrompt is whether the step was an API prompt; meaningful when
	// ParseError is empty.
	APIPrompt bool
	// ParseError is why the step's payload could not be read; empty when it
	// was read.
	ParseError      string
	AcceptedOrdinal *uint64
}

// StartupGroupStatusOf summarizes a group's steps; nil when the group has no
// steps (Rust's StartupGroupStatus::of).
func StartupGroupStatusOf(steps []StartupDelivery) *StartupGroupStatus {
	panic("unported: mj-store/src/database/startup.rs StartupGroupStatus::of")
}

func loadLatestStartupStatusWith(conn Queryer, sessionID string) (*StartupGroupStatus, error) {
	group, err := loadLatestStartupGroupWith(conn, sessionID)
	if err != nil {
		return nil, err
	}
	return StartupGroupStatusOf(group), nil
}

func EnqueueStartupDeliveries(deliveries []StartupDelivery) ([]StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs enqueue_startup_deliveries")
}

func enqueueStartupDeliveriesWith(conn Queryer, deliveries []StartupDelivery) ([]StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs enqueue_startup_deliveries_with")
}

// LoadStartupDelivery reads a step in any phase, including a settled one.
// Callers that only want unfinished work use NextStartupDelivery.
func LoadStartupDelivery(commandID string) (*StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs load_startup_delivery")
}

func PrepareStartupDelivery(commandID string, stepJSON string) error {
	panic("unported: mj-store/src/database/startup.rs prepare_startup_delivery")
}

func LoadStartupDeliveries() ([]StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs load_startup_deliveries")
}

func SetStartupDeliveryPhase(commandID, phase string, err *string) error {
	panic("unported: mj-store/src/database/startup.rs set_startup_delivery_phase")
}

// FailRetainedStartupStep records that a peer message queued during startup
// will not be delivered.
//
// SetStartupDeliveryPhase deletes a failed ungrouped step that is not a user
// prompt. This row has to stay: it is the receipt a retry reads, so the same
// request is not sent again after startup has already failed.
func FailRetainedStartupStep(commandID, err string) error {
	panic("unported: mj-store/src/database/startup.rs fail_retained_startup_step")
}

// ClaimStartupDelivery marks a step as delivering, unless it was withdrawn
// since the drain read it. False means another owner took the step; the
// drain reads the queue again and settles it.
func ClaimStartupDelivery(commandID string) (bool, error) {
	panic("unported: mj-store/src/database/startup.rs claim_startup_delivery")
}

// WithdrawStartupPrompt withdraws exactly one caller-owned prompt before
// worker delivery claims it.
func WithdrawStartupPrompt(sessionID, commandID string) (bool, error) {
	panic("unported: mj-store/src/database/startup.rs withdraw_startup_prompt")
}

// RejectStartupPrompt keeps a definite refusal durable until its complete
// draft is restored. The writer commits draft recovery and row retirement
// together, so a crash never loses input or restores it twice.
func RejectStartupPrompt(commandID, reason string) error {
	panic("unported: mj-store/src/database/startup.rs reject_startup_prompt")
}

func RecoverStartupPrompt(commandID, draft string) error {
	panic("unported: mj-store/src/database/startup.rs recover_startup_prompt")
}

func recoverStartupPromptWith(conn Queryer, commandID, draft string) error {
	panic("unported: mj-store/src/database/startup.rs recover_startup_prompt_with")
}

// rejectFailedMoveStartupPromptsWith turns never-claimed Move prompts into
// recoverable drafts. The Move record and pending queue share the writer
// transaction: only work never claimed by a worker is turned into a
// recoverable draft.
func rejectFailedMoveStartupPromptsWith(conn Queryer, sessionID string) (int, error) {
	panic("unported: mj-store/src/database/startup.rs reject_failed_move_startup_prompts_with")
}

func rejectPendingMoveStartupPromptsWith(conn Queryer, sessionID, moveOperationID, reason string) (int, error) {
	panic("unported: mj-store/src/database/startup.rs reject_pending_move_startup_prompts_with")
}

func RejectPendingMoveStartupPrompts(sessionID, moveOperationID, reason string) error {
	panic("unported: mj-store/src/database/startup.rs reject_pending_move_startup_prompts")
}

// StartupPromptOutcomes reads the terminal outcomes of up to 128 caller-owned
// startup prompts, keyed by command id.
func StartupPromptOutcomes(sessionID string, commandIDs []string) (map[string]client.StartupPromptOutcome, error) {
	panic("unported: mj-store/src/database/startup.rs startup_prompt_outcomes")
}

// LoadLatestStartupGroup is a session's latest startup group, the only one
// the API reports on. The daemon reads it from its published records
// (CommittedState's startup groups); this store read serves a process
// without the writer.
func LoadLatestStartupGroup(sessionID string) ([]StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs load_latest_startup_group")
}

func LoadStartupGroup(sessionID, groupID string) ([]StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs load_startup_group")
}

func loadLatestStartupGroupWith(conn Queryer, sessionID string) ([]StartupDelivery, error) {
	return loadStartupGroupWith(conn, sessionID, nil)
}

func loadStartupGroupWith(conn Queryer, sessionID string, groupID *string) ([]StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs load_startup_group_with")
}

func SetStartupDeliveryAccepted(commandID string, ordinal *uint64) error {
	panic("unported: mj-store/src/database/startup.rs set_startup_delivery_accepted")
}

func FailStartupGroup(sessionID, groupID, err string) error {
	panic("unported: mj-store/src/database/startup.rs fail_startup_group")
}

func CancelStartupGroups(sessionID string) error {
	panic("unported: mj-store/src/database/startup.rs cancel_startup_groups")
}

func DismissStartupGroup(sessionID, groupID string) error {
	panic("unported: mj-store/src/database/startup.rs dismiss_startup_group")
}

func dismissStartupGroupWith(conn Queryer, sessionID, groupID string) error {
	panic("unported: mj-store/src/database/startup.rs dismiss_startup_group_with")
}

// dismissFailedStartupGroupsWith acknowledges settled launch failures on
// retry. A group with even one unfinished step may still need to reconcile
// with its original worker.
func dismissFailedStartupGroupsWith(conn Queryer, sessionID string) error {
	panic("unported: mj-store/src/database/startup.rs dismiss_failed_startup_groups_with")
}

// PruneSettledStartupDeliveries removes rows that nothing will read again:
// dismissed groups, settled legacy steps outside any group, and settled
// groups that a newer group for the same session has superseded (the API
// reads only a session's latest group). Run at daemon start so a store
// upgraded from a build that kept them does not carry them forever. Returns
// how many rows it removed.
func PruneSettledStartupDeliveries() (int, error) {
	panic("unported: mj-store/src/database/startup.rs prune_settled_startup_deliveries")
}

// FailUnavailableStartupGroups rejects pending API input of a session that
// is not starting or live. A late readiness failure must not fail a newer,
// already resumed session. Only pending API input can be rejected:
// delivering rows may be accepted.
func FailUnavailableStartupGroups(sessionID, err string) error {
	panic("unported: mj-store/src/database/startup.rs fail_unavailable_startup_groups")
}

// NextStartupDelivery reads only the first unfinished step using the
// pending-session index.
func NextStartupDelivery(sessionID string) (*StartupDelivery, error) {
	panic("unported: mj-store/src/database/startup.rs next_startup_delivery")
}

// StartupStep is one piece of work that waits for a starting session's
// harness.
//
// Both kinds are ordered against each other on purpose: a restored session's
// hand-off is the hidden context its first prompt reads, so it has to be
// installed before any queued prompt is submitted.
//
// It serializes as serde's externally tagged enum: {"<Variant>": <content>}.
// Decode one with UnmarshalStartupStep.
type StartupStep interface {
	isStartupStep()
}

type StartupStepInstallHandoff struct {
	Value *core.CanonicalSessionSnapshot
}

type StartupStepPreparedHandoff struct {
	Text string `json:"text"`
}

type StartupStepPrompt struct {
	Text string `json:"text"`
	// Prompt is nil only for text rows persisted before structured startup
	// prompts (Rust's None; serialized as null).
	Prompt          []acpschema.ContentBlock `json:"prompt"`
	EncodedDraft    *string                  `json:"encoded_draft"`
	MoveOperationID *string                  `json:"move_operation_id"`
	InheritedDraft  *string                  `json:"inherited_draft"`
}

type StartupStepConfigure struct {
	Key      string `json:"key"`
	Value    string `json:"value"`
	Optional bool   `json:"optional"`
}

type StartupStepApiPrompt struct {
	Text string `json:"text"`
}

// StartupStepSessionMessage is a peer message accepted while this session
// was still starting. Delivered after earlier startup steps: through the
// mailbox when the worker can take one, otherwise as the next turn.
// WaitedOnGroup is the startup group this message queued behind, when one was
// still pending. The request id is the command id's SessionMessageTurn
// suffix.
type StartupStepSessionMessage struct {
	SenderID      string  `json:"sender_id"`
	SenderTitle   string  `json:"sender_title"`
	Text          string  `json:"text"`
	CreatedAtMs   uint64  `json:"created_at_ms"`
	WaitedOnGroup *string `json:"waited_on_group"`
}

func (StartupStepInstallHandoff) isStartupStep()  {}
func (StartupStepPreparedHandoff) isStartupStep() {}
func (StartupStepPrompt) isStartupStep()          {}
func (StartupStepConfigure) isStartupStep()       {}
func (StartupStepApiPrompt) isStartupStep()       {}
func (StartupStepSessionMessage) isStartupStep()  {}

// startupStepJSON writes serde's externally tagged form {"<variant>":content}.
func startupStepJSON(variant string, content any) ([]byte, error) {
	encoded, err := json.Marshal(content)
	if err != nil {
		return nil, err
	}
	name, err := json.Marshal(variant)
	if err != nil {
		return nil, err
	}
	out := make([]byte, 0, len(name)+len(encoded)+3)
	out = append(out, '{')
	out = append(out, name...)
	out = append(out, ':')
	out = append(out, encoded...)
	return append(out, '}'), nil
}

func (v StartupStepInstallHandoff) MarshalJSON() ([]byte, error) {
	return startupStepJSON("InstallHandoff", v.Value)
}

func (v StartupStepPreparedHandoff) MarshalJSON() ([]byte, error) {
	type plain StartupStepPreparedHandoff
	return startupStepJSON("PreparedHandoff", plain(v))
}

func (v StartupStepPrompt) MarshalJSON() ([]byte, error) {
	type plain StartupStepPrompt
	return startupStepJSON("Prompt", plain(v))
}

func (v StartupStepConfigure) MarshalJSON() ([]byte, error) {
	type plain StartupStepConfigure
	return startupStepJSON("Configure", plain(v))
}

func (v StartupStepApiPrompt) MarshalJSON() ([]byte, error) {
	type plain StartupStepApiPrompt
	return startupStepJSON("ApiPrompt", plain(v))
}

func (v StartupStepSessionMessage) MarshalJSON() ([]byte, error) {
	type plain StartupStepSessionMessage
	return startupStepJSON("SessionMessage", plain(v))
}

// startupStepFields decodes a struct variant's content as an object and
// checks that every required field is present, as serde does.
func startupStepFields(content json.RawMessage, variant string, required ...string) (map[string]json.RawMessage, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(content, &fields); err != nil {
		return nil, err
	}
	if fields == nil {
		return nil, fmt.Errorf("invalid type: null, expected struct variant StartupStep::%s", variant)
	}
	for _, name := range required {
		if _, ok := fields[name]; !ok {
			return nil, fmt.Errorf("missing field `%s`", name)
		}
	}
	return fields, nil
}

// UnmarshalStartupStep decodes serde's externally tagged StartupStep.
// Unknown fields inside a variant are ignored, as serde does without
// deny_unknown_fields.
func UnmarshalStartupStep(data []byte) (StartupStep, error) {
	var outer map[string]json.RawMessage
	if err := json.Unmarshal(data, &outer); err != nil {
		return nil, err
	}
	if len(outer) != 1 {
		return nil, errors.New("invalid type: map, expected map with a single key")
	}
	var variant string
	var content json.RawMessage
	for name, value := range outer {
		variant, content = name, value
	}
	switch variant {
	case "InstallHandoff":
		if bytes.Equal(bytes.TrimSpace(content), []byte("null")) {
			return nil, errors.New("invalid type: null, expected struct CanonicalSessionSnapshot")
		}
		snapshot := new(core.CanonicalSessionSnapshot)
		if err := json.Unmarshal(content, snapshot); err != nil {
			return nil, err
		}
		return StartupStepInstallHandoff{Value: snapshot}, nil
	case "PreparedHandoff":
		if _, err := startupStepFields(content, variant, "text"); err != nil {
			return nil, err
		}
		var step StartupStepPreparedHandoff
		type plain StartupStepPreparedHandoff
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "Prompt":
		if _, err := startupStepFields(content, variant, "text"); err != nil {
			return nil, err
		}
		var step StartupStepPrompt
		type plain StartupStepPrompt
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "Configure":
		if _, err := startupStepFields(content, variant, "key", "value", "optional"); err != nil {
			return nil, err
		}
		var step StartupStepConfigure
		type plain StartupStepConfigure
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "ApiPrompt":
		if _, err := startupStepFields(content, variant, "text"); err != nil {
			return nil, err
		}
		var step StartupStepApiPrompt
		type plain StartupStepApiPrompt
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	case "SessionMessage":
		if _, err := startupStepFields(content, variant, "sender_id", "sender_title", "text", "created_at_ms"); err != nil {
			return nil, err
		}
		var step StartupStepSessionMessage
		type plain StartupStepSessionMessage
		if err := json.Unmarshal(content, (*plain)(&step)); err != nil {
			return nil, err
		}
		return step, nil
	default:
		return nil, fmt.Errorf("unknown variant `%s`, expected one of `InstallHandoff`, `PreparedHandoff`, `Prompt`, `Configure`, `ApiPrompt`, `SessionMessage`", variant)
	}
}

// StartFollowup is configuration and a first prompt to apply once a newly
// created session's harness is ready. Served in M2.
type StartFollowup struct {
	Model    *string
	Effort   *string
	Prompt   *string
	FastMode bool
}

// StartupStepID is one startup step's durable command id. The worker relay
// accepts only ASCII alphanumerics, '-' and '_' in a command id, so the step
// key is joined with '-', never ':': a rejected id leaves mj new --model and
// every spawned child waiting forever for a step the worker will never admit.
func StartupStepID(groupID, key string) string {
	return groupID + "-" + key
}

// StartupStepEntry is one of APIFollowupSteps's steps with its command id
// (Rust's (String, StartupStep)).
type StartupStepEntry struct {
	CommandID string
	Step      StartupStep
}

// APIFollowupSteps is the startup steps that apply followup, in order.
func APIFollowupSteps(groupID string, followup StartFollowup) []StartupStepEntry {
	var steps []StartupStepEntry
	for _, setting := range []struct {
		key   string
		value *string
	}{{"model", followup.Model}, {"effort", followup.Effort}} {
		if setting.value != nil {
			steps = append(steps, StartupStepEntry{StartupStepID(groupID, setting.key), StartupStepConfigure{Key: setting.key, Value: *setting.value}})
		}
	}
	if followup.FastMode {
		steps = append(steps, StartupStepEntry{StartupStepID(groupID, "fast-mode"), StartupStepConfigure{Key: "fast-mode", Value: "on", Optional: true}})
	}
	if followup.Prompt != nil {
		steps = append(steps, StartupStepEntry{StartupStepID(groupID, "prompt"), StartupStepApiPrompt{Text: *followup.Prompt}})
	}
	return steps
}
