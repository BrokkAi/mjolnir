// Port of mj-core/src/attention.rs.
//
// Person-facing daemon state. Read receipts belong to clients.

package core

import (
	"encoding/json"
	"fmt"
)

// SessionAttention is what a session needs from the person. The Rust
// default is Unknown.
type SessionAttention string

const (
	SessionAttentionNeedsInput  SessionAttention = "needs_input"
	SessionAttentionImportant   SessionAttention = "important"
	SessionAttentionUnread      SessionAttention = "unread"
	SessionAttentionFailed      SessionAttention = "failed"
	SessionAttentionUnreachable SessionAttention = "unreachable"
	SessionAttentionWorking     SessionAttention = "working"
	SessionAttentionIdle        SessionAttention = "idle"
	SessionAttentionInactive    SessionAttention = "inactive"
	SessionAttentionUnknown     SessionAttention = "unknown"
)

// AssessmentAttention is a semantic assessment result independent of
// continuation scheduling. Today's classifier leaves this unset.
type AssessmentAttention string

const AssessmentAttentionImportant AssessmentAttention = "important"

// NotificationKind is the two notification levels whose card shows a
// message from the agent.
type NotificationKind string

const (
	NotificationKindNeedsInput NotificationKind = "needs_input"
	NotificationKindImportant  NotificationKind = "important"
)

type NotificationStackEntry struct {
	CompletedOrdinal uint64           `json:"completed_ordinal"`
	CompletedAtMs    int64            `json:"completed_at_ms"`
	Kind             NotificationKind `json:"kind"`
	Message          EntryMessage     `json:"message"`
}

func (e *NotificationStackEntry) UnmarshalJSON(data []byte) error {
	type plain NotificationStackEntry
	aux := struct {
		*plain
		Message stateUnionJSON[EntryMessage] `json:"message"`
	}{plain: (*plain)(e)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if aux.Message.V == nil {
		return fmt.Errorf("missing field `message`")
	}
	e.Message = aux.Message.V
	return nil
}

// EntryMessage is externally tagged, snake_case.
type EntryMessage interface {
	isEntryMessage()
}

type EntryMessageLocating struct{}

type EntryMessageFound struct{ Value string }

type EntryMessageNone struct{}

func (EntryMessageLocating) isEntryMessage() {}
func (EntryMessageFound) isEntryMessage()    {}
func (EntryMessageNone) isEntryMessage()     {}

func (EntryMessageLocating) MarshalJSON() ([]byte, error) { return json.Marshal("locating") }

func (v EntryMessageFound) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("found", v.Value)
}

func (EntryMessageNone) MarshalJSON() ([]byte, error) { return json.Marshal("none") }

func unmarshalEntryMessage(data []byte) (EntryMessage, error) {
	tag, content, err := stateExternal(data)
	if err != nil {
		return nil, err
	}
	switch {
	case tag == "locating" && content == nil:
		return EntryMessageLocating{}, nil
	case tag == "none" && content == nil:
		return EntryMessageNone{}, nil
	case tag == "found" && content != nil:
		var text string
		err := json.Unmarshal(content, &text)
		return EntryMessageFound{text}, err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

type NotificationStack struct {
	// Revision is the relay ordinal of the last mutation.
	Revision uint64 `json:"revision"`
	// Entries are oldest first.
	Entries []NotificationStackEntry `json:"entries"`
}

func (s NotificationStack) MarshalJSON() ([]byte, error) {
	type plain NotificationStack
	if s.Entries == nil {
		s.Entries = []NotificationStackEntry{}
	}
	return json.Marshal(plain(s))
}

func (s *NotificationStack) validateAt(frontier uint64) error {
	valid := s.Revision <= frontier
	for i, entry := range s.Entries {
		if !valid {
			break
		}
		valid = entry.CompletedOrdinal > 0 && entry.CompletedOrdinal <= s.Revision &&
			(i == 0 || s.Entries[i-1].CompletedOrdinal < entry.CompletedOrdinal)
	}
	if !valid {
		return fmt.Errorf("notification stack is out of order or beyond its relay frontier")
	}
	return nil
}

// NotificationDismissal is the exact notification identity served by the
// daemon and echoed by clients. Externally tagged, snake_case.
type NotificationDismissal interface {
	isNotificationDismissal()
}

type NotificationDismissalFailure struct{ Value FailureIdentity }

type NotificationDismissalQuestion struct{ Value InferredQuestion }

type NotificationDismissalReadThrough struct{ Value uint64 }

type NotificationDismissalStack struct {
	Revision uint64 `json:"revision"`
}

func (NotificationDismissalFailure) isNotificationDismissal()     {}
func (NotificationDismissalQuestion) isNotificationDismissal()    {}
func (NotificationDismissalReadThrough) isNotificationDismissal() {}
func (NotificationDismissalStack) isNotificationDismissal()       {}

func (v NotificationDismissalFailure) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("failure", v.Value)
}

func (v NotificationDismissalQuestion) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("question", v.Value)
}

func (v NotificationDismissalReadThrough) MarshalJSON() ([]byte, error) {
	return stateMarshalExternal("read_through", v.Value)
}

func (v NotificationDismissalStack) MarshalJSON() ([]byte, error) {
	type plain NotificationDismissalStack
	return stateMarshalExternal("stack", plain(v))
}

func unmarshalNotificationDismissal(data []byte) (NotificationDismissal, error) {
	tag, content, err := stateExternal(data)
	if err != nil {
		return nil, err
	}
	if content == nil {
		return nil, fmt.Errorf("unknown variant `%s`", tag)
	}
	switch tag {
	case "failure":
		var value FailureIdentity
		err := json.Unmarshal(content, &value)
		return NotificationDismissalFailure{value}, err
	case "question":
		var value InferredQuestion
		err := json.Unmarshal(content, &value)
		return NotificationDismissalQuestion{value}, err
	case "read_through":
		var value uint64
		err := json.Unmarshal(content, &value)
		return NotificationDismissalReadThrough{value}, err
	case "stack":
		type plain NotificationDismissalStack
		var value plain
		err := json.Unmarshal(content, &value)
		return NotificationDismissalStack(value), err
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// UnmarshalNotificationDismissal decodes a serialized dismissal.
func UnmarshalNotificationDismissal(data []byte) (NotificationDismissal, error) {
	return unmarshalNotificationDismissal(data)
}

// FailureIdentity is a recorded lifecycle failure, independent of
// transcript progress.
type FailureIdentity struct {
	Generation uint64       `json:"generation"`
	State      SessionState `json:"state"`
	LastError  *string      `json:"last_error"`
}

// FailureIdentityOf is FailureIdentity::of.
func FailureIdentityOf(record *SessionRecord) *FailureIdentity {
	switch {
	case record.LastError != nil,
		record.State == SessionStateError,
		record.State == SessionStateLost,
		record.State == SessionStateDestroyedWithDataLoss:
		return &FailureIdentity{Generation: record.FailureGeneration, State: record.State, LastError: record.LastError}
	}
	return nil
}
