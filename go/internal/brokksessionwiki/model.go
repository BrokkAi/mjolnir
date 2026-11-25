// Port of brokk-sessionwiki 0.34.28 src/model.rs.
//
// Mjolnir never serializes Message, EditEvent or Session itself (it builds
// its own JSON from their fields), so they carry no JSON encoding yet; the
// crate's Serialize forms are only used by the standalone command.

package brokksessionwiki

import (
	"errors"
	"time"
)

// Role is who wrote a message. Serialized in lowercase.
type Role string

const (
	RoleUser      Role = "user"
	RoleAssistant Role = "assistant"
	RoleTool      Role = "tool"
)

// Label is the role's lowercase name.
func (r Role) Label() string { return string(r) }

// ParseRole is Role's FromStr.
func ParseRole(value string) (Role, error) {
	switch role := Role(value); role {
	case RoleUser, RoleAssistant, RoleTool:
		return role, nil
	}
	return "", errors.New("role must be user, assistant, or tool")
}

// Message is one message of a session.
type Message struct {
	Role Role
	Text string
	// Ts is nil when the message has no timestamp.
	Ts *time.Time
	// Tool is the structured tool data, or nil.
	Tool ToolEvent
	// IsThought is true for assistant reasoning summaries.
	IsThought bool
}

// ToolEvent is structured tool data before or after the shared summary pass
// (serde: tag "kind", content "value", snake_case). The variants have pointer
// receivers so a summary can be changed in place through the interface.
type ToolEvent interface {
	isToolEvent()
}

// ToolEventPart is a tool call or result before pairing.
type ToolEventPart struct {
	Part ToolPart
}

// ToolEventSummary is paired tool data after its compact line is rendered.
type ToolEventSummary struct {
	Summary ToolSummary
}

func (*ToolEventPart) isToolEvent()    {}
func (*ToolEventSummary) isToolEvent() {}

// ToolPart is a tool call or result emitted by an adapter before pairing.
type ToolPart interface {
	isToolPart()
}

type ToolPartCall struct {
	Call ToolCall
}

type ToolPartResult struct {
	Result ToolResult
}

func (ToolPartCall) isToolPart()   {}
func (ToolPartResult) isToolPart() {}

// ToolCall is SessionWiki's model of a tool call (not ACP's).
type ToolCall struct {
	ID   *string
	Name string
	// Args is a JSON value in acp-go's jsonvalue representation.
	Args any
}

type ToolResult struct {
	CallID  *string
	Text    string
	IsError bool
}

// ToolSummary is paired tool data retained in memory after its compact line
// is rendered.
type ToolSummary struct {
	Name string
	// Args is a JSON value in acp-go's jsonvalue representation.
	Args    any
	Output  *string
	IsError *bool
	Lines   *int
}

// EditKind is the write tool behind one file edit. Serialized in lowercase.
type EditKind string

const (
	EditKindEdit         EditKind = "edit"
	EditKindWrite        EditKind = "write"
	EditKindMultiEdit    EditKind = "multiedit"
	EditKindNotebookEdit EditKind = "notebookedit"
)

// AsStr is the stable lowercase token stored in the index.
func (k EditKind) AsStr() string { return string(k) }

// EditEvent is one concrete edit a session made to one file.
type EditEvent struct {
	Path    string
	Kind    EditKind
	Snippet string
	Ts      *time.Time
}

// Session is one parsed session.
type Session struct {
	// ID is a short stable id derived from the file path (FNV-1a hash, hex).
	ID      string
	Tool    string
	Path    string
	Project string
	// RawCwd is the literal native working directory, when parsing the
	// source provides it.
	RawCwd *string
	// OperationalEditedPaths are literal paths backed by the adapter's
	// native write evidence.
	OperationalEditedPaths []string
	// CompactedPrefix: the source begins with compacted history before its
	// first real prompt.
	CompactedPrefix bool
	Started         *time.Time
	Ended           *time.Time
	Title           string
	// Subagent is true for subagent transcripts spawned inside a parent
	// session.
	Subagent bool
	Messages []Message
	// Touched is the files the session edited or created.
	Touched []string
	// Edits is the concrete edits behind Touched, per file.
	Edits []EditEvent
}
