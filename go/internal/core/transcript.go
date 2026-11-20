// Port of mj-core/src/transcript.rs.
//
// Transcript data shared by the worker, controller state, and the chat UI.
//
// ChatEntry is what a worker snapshot carries in its transcript tail and
// what the chat view renders, and TranscriptItem is the materialized form
// controller state persists, so both live below the modules that use them
// rather than inside any one of them.
//
// The text helpers that read one of those shapes live here for the same
// reason: the database, the projection, controller state, the compactor and
// the review host all need the plain text of a stored message, and none of
// them should have to reach up into the chat view to get it.
//
// serde_json::Value is held as json.RawMessage. serde_json is built with
// preserve_order in the Rust workspace, so an object's members keep their
// wire order; parse with github.com/BrokkAi/acp-go/jsonvalue to inspect one.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"sort"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acp "github.com/BrokkAi/acp-go/schema/unstable"
)

// AdoptionHistoryUnavailableItemID marks that earlier transcript evidence was
// lost when the daemon adopted a live worker. Until the archive carries
// native-use proof, this marker prevents an empty native capture from
// claiming that missing history proves an unused session.
const AdoptionHistoryUnavailableItemID = "system:adoption-history-unavailable"

const SessionRestartText = "[session restarted]"
const SessionRestartItemPrefix = "system:session-restarted:"
const WorkInterruptedItemPrefix = "system:work-interrupted:"

// TurnInterruptedText marks where a turn the user cancelled stopped.
const TurnInterruptedText = "Interrupted"
const TurnInterruptedItemPrefix = "system:turn-interrupted:"

// HarnessTurnText marks the point where the harness resumed work with no
// prompt in flight.
const HarnessTurnText = "Agent continued on its own"
const HarnessTurnItemPrefix = "harness-turn:"

// ElicitationReplyItemPrefix marks user replies within an existing turn,
// rather than prompts that start one.
const ElicitationReplyItemPrefix = "elicitation-reply:"

// TerminalOutputs is the shared read-time join of embedded and parked
// terminal output. Standalone close-time records override embedded streaming
// records.
type TerminalOutputs[R TerminalRecord] struct {
	standalone map[string]R
	tools      map[string]terminalOutputsTool[R]
	referenced map[string]struct{}
}

// terminalOutputsTool is one tool item's embedded records and references.
type terminalOutputsTool[R TerminalRecord] struct {
	embedded []R
	refs     []string
}

// TerminalRecord is a record of one terminal's output. The Rust trait method
// terminal_id is GetTerminalID here because the records also have a
// TerminalID field.
type TerminalRecord interface {
	GetTerminalID() string
}

func (r TerminalOutputRecord) GetTerminalID() string    { return r.TerminalID }
func (r CanonicalTerminalOutput) GetTerminalID() string { return r.TerminalID }

func newTerminalOutputs[R TerminalRecord]() *TerminalOutputs[R] {
	return &TerminalOutputs[R]{
		standalone: map[string]R{},
		tools:      map[string]terminalOutputsTool[R]{},
		referenced: map[string]struct{}{},
	}
}

func (o *TerminalOutputs[R]) tool(id string, embedded []R, refs []string) {
	o.tools[id] = terminalOutputsTool[R]{embedded: embedded, refs: refs}
	for _, ref := range refs {
		o.referenced[ref] = struct{}{}
	}
	for _, record := range embedded {
		o.referenced[record.GetTerminalID()] = struct{}{}
	}
}

// ForTool is the output shown under the tool item stableID.
func (o *TerminalOutputs[R]) ForTool(stableID string) []R {
	panic("unported: mj-core/src/transcript.rs TerminalOutputs::for_tool")
}

func (o *TerminalOutputs[R]) EmbeddedForTool(stableID string) []R {
	return o.tools[stableID].embedded
}

// ReferencedTerminalIDs lists every terminal some tool item refers to.
func (o *TerminalOutputs[R]) ReferencedTerminalIDs() []string {
	ids := make([]string, 0, len(o.referenced))
	for id := range o.referenced {
		ids = append(ids, id)
	}
	return ids
}

func (o *TerminalOutputs[R]) HasStandalone() bool { return len(o.standalone) != 0 }

func (o *TerminalOutputs[R]) ShownUnderTool(terminalID string) bool {
	_, ok := o.referenced[terminalID]
	return ok
}

// TerminalOutputsOf joins the terminal output of materialized items.
func TerminalOutputsOf(items []TranscriptItem) *TerminalOutputs[TerminalOutputRecord] {
	outputs := newTerminalOutputs[TerminalOutputRecord]()
	for i := range items {
		switch body := items[i].Body.(type) {
		case TranscriptBodyTool:
			outputs.tool(items[i].StableID, body.TerminalOutputs, body.TerminalRefs)
		case TranscriptBodyTerminalOutput:
			outputs.standalone[body.Record.TerminalID] = body.Record
		}
	}
	return outputs
}

// TerminalOutputsOfCanonical joins the terminal output of archived items.
func TerminalOutputsOfCanonical(items []CanonicalTranscriptItem) *TerminalOutputs[CanonicalTerminalOutput] {
	outputs := newTerminalOutputs[CanonicalTerminalOutput]()
	for i := range items {
		switch body := items[i].Body.(type) {
		case CanonicalTranscriptBodyTool:
			outputs.tool(items[i].StableID, body.TerminalOutputs, body.TerminalRefs)
		case CanonicalTranscriptBodyTerminalOutput:
			outputs.standalone[body.Record.TerminalID] = body.Record
		}
	}
	return outputs
}

// ToolSummarySourceKind says where a tool's compact presentation source came from.
type ToolSummarySourceKind string

const (
	ToolSummarySourceKindRawInput  ToolSummarySourceKind = "raw_input"
	ToolSummarySourceKindRawOutput ToolSummarySourceKind = "raw_output"
	ToolSummarySourceKindTitle     ToolSummarySourceKind = "title"
)

// ToolCallPresentation is bounded presentation metadata derived from an ACP
// tool call.
type ToolCallPresentation struct {
	Summary    string                `json:"summary"`
	Source     PackedText            `json:"source"`
	SourceKind ToolSummarySourceKind `json:"source_kind"`
	ToolKind   acp.ToolKind          `json:"tool_kind"`
	// SummaryVersion is the version of the parser rules that produced
	// Summary. A missing value identifies presentation metadata written
	// before parser versioning.
	SummaryVersion uint8 `json:"summary_version" serde:"default"`
}

func (p *ToolCallPresentation) UnmarshalJSON(data []byte) error {
	type plain ToolCallPresentation
	return transcriptStrictDecode(data, (*plain)(p), false)
}

// OpaqueToolCall is an ACP call retained as JSON text. All cached
// interpretations are built together; replacing the call cannot leave
// metadata from an older update.
type OpaqueToolCall struct {
	json     PackedText
	metadata *toolCallMetadata
}

type toolCallMetadata struct {
	rawStatus   *string
	typedStatus *acp.ToolCallStatus
	id          *string
	terminalIDs []string
	fallback    bool
	rawResult   *toolCallRawResult
}

// toolCallRawResult is the exit code, output length and output digest of a
// completed call's provider-specific raw result.
type toolCallRawResult struct {
	exit   uint32
	len    int
	digest [32]byte
}

// MarshalJSON writes the retained JSON text verbatim.
func (c OpaqueToolCall) MarshalJSON() ([]byte, error) {
	return []byte(c.json.Text()), nil
}

// UnmarshalJSON reads any JSON value, as serde's from = "serde_json::Value".
func (c *OpaqueToolCall) UnmarshalJSON(data []byte) error {
	value, err := transcriptNormalizeValue(data)
	if err != nil {
		return err
	}
	*c = OpaqueToolCallFromValue(value)
	return nil
}

// Equal is the Rust PartialEq: equal text, or equal values.
func (c OpaqueToolCall) Equal(other OpaqueToolCall) bool {
	panic("unported: mj-core/src/transcript.rs OpaqueToolCall::eq")
}

// OpaqueToolCallFromValue builds the call and its cached metadata from a
// JSON value.
func OpaqueToolCallFromValue(value json.RawMessage) OpaqueToolCall {
	panic("unported: mj-core/src/transcript.rs OpaqueToolCall::from")
}

// Edit applies edit to the call's value (in jsonvalue form) and rebuilds the
// cached metadata from the result.
func (c *OpaqueToolCall) Edit(edit func(value *any)) {
	panic("unported: mj-core/src/transcript.rs OpaqueToolCall::edit")
}

func (c OpaqueToolCall) Value() json.RawMessage {
	return json.RawMessage(c.json.Text())
}

func (c OpaqueToolCall) Decode() (acp.ToolCall, error) {
	var call acp.ToolCall
	err := json.Unmarshal([]byte(c.json.Text()), &call)
	return call, err
}

func (c OpaqueToolCall) RawStatus() (string, bool) {
	if c.metadata == nil || c.metadata.rawStatus == nil {
		return "", false
	}
	return *c.metadata.rawStatus, true
}

func (c OpaqueToolCall) TypedStatus() acp.ToolCallStatus {
	if c.metadata == nil || c.metadata.typedStatus == nil {
		return acp.ToolCallStatusPending
	}
	return *c.metadata.typedStatus
}

func (c OpaqueToolCall) TypedValid() bool {
	return c.metadata != nil && c.metadata.typedStatus != nil
}

func (c OpaqueToolCall) ID() (string, bool) {
	if c.metadata == nil || c.metadata.id == nil {
		return "", false
	}
	return *c.metadata.id, true
}

func (c OpaqueToolCall) TerminalIDs() []string {
	if c.metadata == nil {
		return nil
	}
	return c.metadata.terminalIDs
}

func (c OpaqueToolCall) IsFallback() bool {
	return c.metadata != nil && c.metadata.fallback
}

func (c OpaqueToolCall) MatchesRawResult(record *TerminalOutputRecord, digest *[32]byte) bool {
	panic("unported: mj-core/src/transcript.rs OpaqueToolCall::matches_raw_result")
}

// ToolCallTerminalIDs is the one interpretation of ACP terminal references
// for opaque metadata and transient JSON readers (for example summaries and
// relay update fields).
func ToolCallTerminalIDs(call json.RawMessage) []string {
	panic("unported: mj-core/src/transcript.rs tool_call_terminal_ids")
}

// TranscriptBody is the current value of one logical transcript item. ACP
// structures whose schemas can grow are kept as JSON values, while logical
// item identity and lifecycle remain controller-owned and stable.
//
// Serialized internally tagged by "kind". Decode with DecodeTranscriptBody.
type TranscriptBody interface {
	isTranscriptBody()
}

// TranscriptBodyMessage is a complete message at the worker's durable
// delivery boundary.
type TranscriptBodyMessage struct {
	Event *MailboxEvent `json:"event"`
}

type TranscriptBodyUser struct {
	Content []json.RawMessage `json:"content"`
}

type TranscriptBodyAgent struct {
	// Chunks are complete ACP ContentChunk values, including message IDs,
	// content metadata, and non-text content blocks.
	Chunks    []json.RawMessage `json:"chunks"`
	Streaming bool              `json:"streaming"`
}

type TranscriptBodyThought struct {
	// Chunks are complete ACP ContentChunk values, including message IDs,
	// content metadata, and non-text content blocks.
	Chunks    []json.RawMessage `json:"chunks"`
	Streaming bool              `json:"streaming"`
}

type TranscriptBodyTool struct {
	// Call is the complete current ACP ToolCall, updated field-for-field as
	// ToolCallUpdate notifications arrive.
	Call OpaqueToolCall `json:"call"`
	// TerminalOutputs is the output of the terminals this call's content
	// refers to. It is a sibling of Call rather than part of it because
	// ToolCall::update replaces content wholesale, which would discard
	// anything injected into the stored ACP value.
	TerminalOutputs []TerminalOutputRecord `json:"terminal_outputs,omitempty"`
	// TerminalRefs is every terminal this call has ever referred to. Agents
	// that replace content wholesale can drop a terminal reference before
	// the terminal is reaped, so the current call is not enough to decide
	// where a terminal's output belongs.
	TerminalRefs []string `json:"terminal_refs,omitempty"`
	// Presentation is cached label data used by Rich and browser
	// projections. Older transcript items omit this and derive it from Call
	// when read.
	Presentation *ToolCallPresentation `json:"presentation,omitempty"`
}

// TranscriptBodyTerminalOutput is terminal output that no tool call refers
// to yet. It becomes a Tool item's TerminalOutputs entry as soon as a call
// naming the terminal arrives, and stays here permanently otherwise so
// output is never dropped.
type TranscriptBodyTerminalOutput struct {
	Record TerminalOutputRecord `json:"record"`
}

type TranscriptBodyPlan struct {
	// Plan is the complete current ACP Plan, including entry priorities and
	// all plan- and entry-level metadata.
	Plan json.RawMessage `json:"plan"`
}

// TranscriptBodyPlanProposal is a plan the harness asked the user to
// approve, captured where the decision happened so it renders inline and
// survives restart and export.
//
// It is a record of the proposal, not conversation input: Hel never replays
// it to a model as a user or agent message.
type TranscriptBodyPlanProposal struct {
	// ProposalID is the identity of the plan review that carried this proposal.
	ProposalID string `json:"proposal_id"`
	// Plan is the exact proposal text the harness sent.
	Plan string `json:"plan"`
}

type TranscriptBodySystem struct {
	Text string `json:"text"`
}

func (TranscriptBodyMessage) isTranscriptBody()        {}
func (TranscriptBodyUser) isTranscriptBody()           {}
func (TranscriptBodyAgent) isTranscriptBody()          {}
func (TranscriptBodyThought) isTranscriptBody()        {}
func (TranscriptBodyTool) isTranscriptBody()           {}
func (TranscriptBodyTerminalOutput) isTranscriptBody() {}
func (TranscriptBodyPlan) isTranscriptBody()           {}
func (TranscriptBodyPlanProposal) isTranscriptBody()   {}
func (TranscriptBodySystem) isTranscriptBody()         {}

func (b TranscriptBodyMessage) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyMessage
	return transcriptMarshalTagged("kind", "message", plain(b))
}

func (b TranscriptBodyUser) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyUser
	return transcriptMarshalTagged("kind", "user", plain(b))
}

func (b TranscriptBodyAgent) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyAgent
	return transcriptMarshalTagged("kind", "agent", plain(b))
}

func (b TranscriptBodyThought) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyThought
	return transcriptMarshalTagged("kind", "thought", plain(b))
}

func (b TranscriptBodyTool) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyTool
	return transcriptMarshalTagged("kind", "tool", plain(b))
}

func (b TranscriptBodyTerminalOutput) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyTerminalOutput
	return transcriptMarshalTagged("kind", "terminal_output", plain(b))
}

func (b TranscriptBodyPlan) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyPlan
	return transcriptMarshalTagged("kind", "plan", plain(b))
}

func (b TranscriptBodyPlanProposal) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodyPlanProposal
	return transcriptMarshalTagged("kind", "plan_proposal", plain(b))
}

func (b TranscriptBodySystem) MarshalJSON() ([]byte, error) {
	type plain TranscriptBodySystem
	return transcriptMarshalTagged("kind", "system", plain(b))
}

// DecodeTranscriptBody reads a TranscriptBody, tagged by "kind".
func DecodeTranscriptBody(data []byte) (TranscriptBody, error) {
	tag, err := transcriptTag(data, "kind", "TranscriptBody")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "message":
		type plain TranscriptBodyMessage
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyMessage(v), err
	case "user":
		type plain TranscriptBodyUser
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyUser(v), err
	case "agent":
		type plain TranscriptBodyAgent
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyAgent(v), err
	case "thought":
		type plain TranscriptBodyThought
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyThought(v), err
	case "tool":
		type plain TranscriptBodyTool
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyTool(v), err
	case "terminal_output":
		type plain TranscriptBodyTerminalOutput
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyTerminalOutput(v), err
	case "plan":
		type plain TranscriptBodyPlan
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyPlan(v), err
	case "plan_proposal":
		type plain TranscriptBodyPlanProposal
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodyPlanProposal(v), err
	case "system":
		type plain TranscriptBodySystem
		var v plain
		err = transcriptStrictDecode(data, &v, false, "kind")
		return TranscriptBodySystem(v), err
	}
	return nil, transcriptUnknownVariant(tag, "message", "user", "agent", "thought", "tool",
		"terminal_output", "plan", "plan_proposal", "system")
}

// PushContentChunk appends one ACP ContentChunk value to a transcript item's
// chunk list, merging it into the previous chunk when the two are the same
// text stream.
//
// Agents stream text one token at a time, so a long turn arrives as
// thousands of chunks that differ only in content.text. Stored separately
// they cost far more memory than the text they carry. Merging them keeps one
// chunk per run of text, which is what every reader of chunks already
// reconstructs.
//
// Two chunks merge only when nothing but the text differs: both are objects
// whose content.type is "text" with a string content.text, their messageId
// values are equal (both absent counts as equal), and every other top-level
// key (such as meta) and every other content key (such as annotations) is
// identical. Anything else is pushed as its own chunk.
func PushContentChunk(chunks *[]json.RawMessage, chunk json.RawMessage) {
	panic("unported: mj-core/src/transcript.rs push_content_chunk")
}

// CoalesceContentChunks collapses runs of per-token text chunks that were
// stored before they were merged on the way in. Rebuilds the list through
// PushContentChunk, so it applies exactly the same merge rule and leaves
// chunk boundaries that carry real differences (a new message ID, non-text
// content, differing metadata) where they are.
func CoalesceContentChunks(chunks *[]json.RawMessage) {
	if len(*chunks) < 2 {
		return
	}
	merged := make([]json.RawMessage, 0, len(*chunks))
	for _, chunk := range *chunks {
		PushContentChunk(&merged, chunk)
	}
	*chunks = merged
}

// textChunksMergeable reports whether next carries only more text for the
// same stream as last, so the two can share one chunk. The single
// definition of the merge rule used by both PushContentChunk and
// CoalesceContentChunks.
func textChunksMergeable(last, next any) bool {
	panic("unported: mj-core/src/transcript.rs text_chunks_mergeable")
}

// TerminalOutputRecord is what one client-run terminal produced, as hel
// recorded it when the child was reaped. ExitCode and Signal mirror ACP
// TerminalExitStatus; both are nil when the terminal was released before a
// status was observed.
type TerminalOutputRecord struct {
	TerminalID string     `json:"terminal_id"`
	Output     PackedText `json:"output"`
	Truncated  bool       `json:"truncated,omitempty"`
	ExitCode   *uint32    `json:"exit_code,omitempty"`
	Signal     *string    `json:"signal,omitempty"`
}

func (r *TerminalOutputRecord) UnmarshalJSON(data []byte) error {
	type plain TerminalOutputRecord
	return transcriptStrictDecode(data, (*plain)(r), false)
}

// RawResultDigest is computed once before comparing one terminal event with
// the tool window.
func (r *TerminalOutputRecord) RawResultDigest() [32]byte {
	panic("unported: mj-core/src/transcript.rs TerminalOutputRecord::raw_result_digest")
}

// ExitedCleanly reports whether the command ended the way a caller asked
// for: exit status zero and no signal. Anything else (a nonzero exit, a
// signal, or no status at all because the terminal was released before one
// was observed) is abnormal, and stays visible in every render mode.
func (r *TerminalOutputRecord) ExitedCleanly() bool {
	return r.ExitCode != nil && *r.ExitCode == 0 && r.Signal == nil
}

// MatchesToolRawResult reports whether a completed ACP tool's
// provider-specific raw result is this child result. Kimi reports shell
// output as a byte array beside its exit status but omits the ACP terminal
// reference, so the exact result is the only ownership information it
// publishes.
func (r *TerminalOutputRecord) MatchesToolRawResult(call json.RawMessage) bool {
	panic("unported: mj-core/src/transcript.rs TerminalOutputRecord::matches_tool_raw_result")
}

// TranscriptItem is one materialized logical transcript item.
type TranscriptItem struct {
	StableID string `json:"stable_id"`
	// Position is the ordinal of the relay event that first created this
	// logical item.
	Position uint64 `json:"position"`
	// LatestContentEventOrdinal is the ordinal of the most recent content
	// chunk for an agent message. It is nil for every other logical item.
	LatestContentEventOrdinal *uint64        `json:"latest_content_event_ordinal"`
	CreatedAtMs               int64          `json:"created_at_ms"`
	LastChangedAtMs           int64          `json:"last_changed_at_ms"`
	Body                      TranscriptBody `json:"body"`
}

// UnmarshalJSON applies deny_unknown_fields and decodes the tagged body.
func (t *TranscriptItem) UnmarshalJSON(data []byte) error {
	type plain struct {
		StableID                  string          `json:"stable_id"`
		Position                  uint64          `json:"position"`
		LatestContentEventOrdinal *uint64         `json:"latest_content_event_ordinal"`
		CreatedAtMs               int64           `json:"created_at_ms"`
		LastChangedAtMs           int64           `json:"last_changed_at_ms"`
		Body                      json.RawMessage `json:"body"`
	}
	var v plain
	if err := transcriptStrictDecode(data, &v, true); err != nil {
		return err
	}
	body, err := DecodeTranscriptBody(v.Body)
	if err != nil {
		return err
	}
	*t = TranscriptItem{v.StableID, v.Position, v.LatestContentEventOrdinal, v.CreatedAtMs, v.LastChangedAtMs, body}
	return nil
}

func (t *TranscriptItem) IsWorkInterruption() bool {
	return strings.HasPrefix(t.StableID, WorkInterruptedItemPrefix)
}

func (t *TranscriptItem) IsSessionRestart() bool {
	return strings.HasPrefix(t.StableID, SessionRestartItemPrefix)
}

// Seq is the relay ordinal a reader pages by.
//
// An agent message is rewritten as its content streams in, and its
// LatestContentEventOrdinal is where that stopped, so paging by it hands a
// caller the finished message once instead of the partial one it was
// created with. Every other body is created once, so its position is its
// sequence.
func (t *TranscriptItem) Seq() uint64 {
	if t.LatestContentEventOrdinal != nil {
		return *t.LatestContentEventOrdinal
	}
	return t.Position
}

// IsUserPrompt reports a submitted prompt, excluding replies within an
// existing turn.
func (t *TranscriptItem) IsUserPrompt() bool {
	_, user := t.Body.(TranscriptBodyUser)
	return user && !strings.HasPrefix(t.StableID, ElicitationReplyItemPrefix)
}

// IsTurnStart reports whether this item begins a turn: a user prompt, or the
// marker for a turn the harness started on its own. The recovery boundary
// and the scope of a plan update both key on the newest of these.
func (t *TranscriptItem) IsTurnStart() bool {
	return t.IsUserPrompt() || strings.HasPrefix(t.StableID, HarnessTurnItemPrefix)
}

func (t *TranscriptItem) IsNonemptyAgentMessage() bool {
	panic("unported: mj-core/src/transcript.rs TranscriptItem::is_nonempty_agent_message")
}

func (t *TranscriptItem) Validate(through uint64) error {
	panic("unported: mj-core/src/transcript.rs TranscriptItem::validate")
}

// ChatRole is the chat view's role for an entry. It serializes with the Rust
// variant names.
type ChatRole string

const (
	ChatRoleUser    ChatRole = "User"
	ChatRoleAgent   ChatRole = "Agent"
	ChatRoleMessage ChatRole = "Message"
	// ChatRoleThought is the agent reasoning stream, rendered dimmed.
	ChatRoleThought ChatRole = "Thought"
	// ChatRoleTool is for tool invocation titles.
	ChatRoleTool ChatRole = "Tool"
	// ChatRolePlan is the current agent plan.
	ChatRolePlan ChatRole = "Plan"
	// ChatRolePlanProposal is a plan proposal awaiting, or already given, a decision.
	ChatRolePlanProposal ChatRole = "PlanProposal"
	ChatRoleSystem       ChatRole = "System"
)

type ChatEntry struct {
	StartSeq     uint64   `json:"start_seq" serde:"default"`
	Seq          uint64   `json:"seq"`
	Role         ChatRole `json:"role"`
	Text         string   `json:"text"`
	RecordedAtMs *int64   `json:"recorded_at_ms"`
	// ToolEndedAtMs is when a finished tool call last changed, which is when
	// it ended. Nil for a call still running and for every other role.
	ToolEndedAtMs   *int64        `json:"tool_ended_at_ms,omitempty"`
	Revision        uint64        `json:"revision"`
	MessageID       *string       `json:"message_id"`
	IncomingMessage *MailboxEvent `json:"incoming_message,omitempty"`
	ToolCallID      *string       `json:"tool_call_id"`
	ToolStatus      *ToolStatus   `json:"tool_status"`
	// ToolSummary is the compact label used by Rich and browser projections.
	// Text remains the original provider title for Raw mode.
	ToolSummary *string `json:"tool_summary,omitempty"`
	// ToolPresentation is the selected bounded source retained so partial
	// ACP updates can preserve a raw-command-derived summary without
	// retaining arbitrary raw JSON.
	ToolPresentation *ToolCallPresentation `json:"tool_presentation,omitempty"`
	// ToolInput holds the ACP arguments, kept separately so result updates
	// cannot erase them.
	ToolInput json.RawMessage `json:"tool_input,omitempty"`
	// ToolError is a diagnostic reported in ACP rawOutput.error, kept
	// separate from textual tool content so partial result updates can
	// replace it.
	ToolError      *string    `json:"tool_error,omitempty"`
	ToolContent    []string   `json:"tool_content"`
	ToolDiffstats  []string   `json:"tool_diffstats"`
	ToolLocations  []string   `json:"tool_locations"`
	Plan           []PlanLine `json:"plan"`
	LeadingOmitted bool       `json:"leading_omitted,omitempty"`
	// RawOnly is detail the decluttered feed leaves out: the entry renders
	// only in the raw transcript mode. Set once, when the entry is built,
	// because Alt-T switches render mode without rebuilding entries.
	RawOnly bool `json:"raw_only,omitempty"`
	// EndedAfterInterrupt marks a completed tool call whose turn was
	// interrupted while it ran. The harness kept the command going and it
	// ended later on its own, so its completion is not the agent's finished
	// work.
	EndedAfterInterrupt bool `json:"ended_after_interrupt,omitempty"`
	// Source is the materialized transcript item this entry was derived
	// from, when it came from the controller's projection. Provenance only,
	// so it is neither serialized nor part of the entry's value.
	Source TranscriptSource `json:"-"`
}

func (e *ChatEntry) UnmarshalJSON(data []byte) error {
	type plain ChatEntry
	return transcriptStrictDecode(data, (*plain)(e), false)
}

// TranscriptSource is a handle on the transcript item an entry was derived
// from. Unchanged items keep the same pointer from one projection to the
// next, so a pointer comparison replaces re-reading the item and re-parsing
// its JSON.
//
// The handle records where an entry came from, not what it says, so two
// entries with equal content are equal whatever they were derived from.
type TranscriptSource struct {
	Item *TranscriptItem
}

func (s TranscriptSource) Is(item *TranscriptItem) bool {
	return s.Item != nil && s.Item == item
}

// Equal is always true: provenance is not part of an entry's value.
func (s TranscriptSource) Equal(TranscriptSource) bool { return true }

func ChatEntryReceivedMessage(seq uint64, event *MailboxEvent) ChatEntry {
	panic("unported: mj-core/src/transcript.rs ChatEntry::received_message")
}

// IsSessionRestart reports whether this entry is the durable marker emitted
// when a session's control plane restarts. The source identity is
// authoritative for materialized entries; the role/text check also covers
// entries built from older worker snapshots that have no materialized
// source handle.
func (e *ChatEntry) IsSessionRestart() bool {
	return (e.Source.Item != nil && e.Source.Item.IsSessionRestart()) ||
		(e.Role == ChatRoleSystem && e.Text == SessionRestartText)
}

// IsUserAuthored reports whether this entry is a message the user wrote,
// including an answer to an agent's question. Prompts that Mjolnir submits
// on the user's behalf are excluded: every continuation GeneratedPrompt and
// text marked as generated. Authorization evidence asks the same two
// predicates, so the two cannot disagree.
func (e *ChatEntry) IsUserAuthored() bool {
	panic("unported: mj-core/src/transcript.rs ChatEntry::is_user_authored")
}

func ChatEntryPlan(seq uint64, plan []PlanLine) ChatEntry {
	return ChatEntry{
		StartSeq:      seq,
		Seq:           seq,
		Role:          ChatRolePlan,
		ToolContent:   []string{},
		ToolDiffstats: []string{},
		ToolLocations: []string{},
		Plan:          plan,
	}
}

func (e *ChatEntry) Touch(seq uint64) {
	e.Seq = seq
	e.Revision++
}

// BoundedForDashboard bounds one entry to the sizes the dashboard's summary
// tolerates.
func (e ChatEntry) BoundedForDashboard() ChatEntry {
	e.boundDashboardContent()
	return e
}

func (e *ChatEntry) boundDashboardContent() {
	panic("unported: mj-core/src/transcript.rs ChatEntry::bound_dashboard_content")
}

func (e ChatEntry) WithRecordedAt(recordedAtMs *int64) ChatEntry {
	e.RecordedAtMs = recordedAtMs
	return e
}

// ChatEntryPlain builds an entry, sanitizing text so terminal escape
// sequences from a harness never reach a transcript entry.
func ChatEntryPlain(seq uint64, role ChatRole, text string) ChatEntry {
	return ChatEntry{
		StartSeq:      seq,
		Seq:           seq,
		Role:          role,
		Text:          SanitizeTerminalText(text),
		ToolContent:   []string{},
		ToolDiffstats: []string{},
		ToolLocations: []string{},
		Plan:          []PlanLine{},
	}
}

// ChatEntryTool builds a tool entry, sanitizing its title.
func ChatEntryTool(seq uint64, title string, toolCallID *string, toolStatus ToolStatus) ChatEntry {
	entry := ChatEntryPlain(seq, ChatRoleTool, title)
	entry.ToolCallID = toolCallID
	entry.ToolStatus = &toolStatus
	return entry
}

// transcriptIsFalse is serde's skip_serializing_if helper; Go's omitempty
// does its work.
func transcriptIsFalse(value bool) bool { return !value }

// PlanStatusOf maps an ACP plan entry status. (Rust plan_status; renamed
// because PlanStatus is the type.)
func PlanStatusOf(status acp.PlanEntryStatus) PlanStatus {
	switch status {
	case acp.PlanEntryStatusInProgress:
		return PlanStatusRunning
	case acp.PlanEntryStatusCompleted:
		return PlanStatusCompleted
	}
	return PlanStatusPending
}

// SanitizeTerminalText removes terminal controls while preserving
// user-visible whitespace.
func SanitizeTerminalText(text string) string {
	panic("unported: mj-core/src/transcript.rs sanitize_terminal_text")
}

// consumeEscapeBody consumes one escape sequence's body, after its
// introducing ESC, from chars starting at index i. It returns the next index
// and whether the body ended at another ESC, which introduces the next
// sequence.
//
// Dropping the ESC alone is not enough: an OSC payload (a build tool setting
// the window title) or the second byte of a charset selection would
// otherwise reach the transcript as visible text.
func consumeEscapeBody(chars []rune, i int) (int, bool) {
	panic("unported: mj-core/src/transcript.rs consume_escape_body")
}

// consumeStringBody consumes a string payload, which ends at BEL or at ST
// (ESC \). A line break or a cancel control aborts it instead, so one
// malformed OSC cannot swallow the rest of a transcript.
func consumeStringBody(chars []rune, i int) (int, bool) {
	panic("unported: mj-core/src/transcript.rs consume_string_body")
}

func MaterializedContentText(content []json.RawMessage) string {
	panic("unported: mj-core/src/transcript.rs materialized_content_text")
}

// TranscriptRole is what produced a transcript item, as a stable wire name.
//
// The chat view has its own role enum shaped around how it renders; this is
// the name the HTTP API publishes, so it changes only when the transcript
// model does.
type TranscriptRole string

const (
	TranscriptRoleUser         TranscriptRole = "user"
	TranscriptRoleAgent        TranscriptRole = "agent"
	TranscriptRoleMessage      TranscriptRole = "message"
	TranscriptRoleThought      TranscriptRole = "thought"
	TranscriptRoleTool         TranscriptRole = "tool"
	TranscriptRoleTerminal     TranscriptRole = "terminal"
	TranscriptRolePlan         TranscriptRole = "plan"
	TranscriptRolePlanProposal TranscriptRole = "plan_proposal"
	TranscriptRoleSystem       TranscriptRole = "system"
)

func (r TranscriptRole) AsStr() string { return string(r) }

func (r TranscriptRole) StorageKind() string {
	if r == TranscriptRoleTerminal {
		return "terminal_output"
	}
	return r.AsStr()
}

func TranscriptItemRole(body TranscriptBody) string {
	var role TranscriptRole
	switch body.(type) {
	case TranscriptBodyUser:
		role = TranscriptRoleUser
	case TranscriptBodyAgent:
		role = TranscriptRoleAgent
	case TranscriptBodyMessage:
		role = TranscriptRoleMessage
	case TranscriptBodyThought:
		role = TranscriptRoleThought
	case TranscriptBodyTool:
		role = TranscriptRoleTool
	case TranscriptBodyTerminalOutput:
		role = TranscriptRoleTerminal
	case TranscriptBodyPlan:
		role = TranscriptRolePlan
	case TranscriptBodyPlanProposal:
		role = TranscriptRolePlanProposal
	case TranscriptBodySystem:
		role = TranscriptRoleSystem
	}
	return role.AsStr()
}

func MessageItemID(eventKey string) string { return "message:" + eventKey }

// MaterializedAgentMessageText recovers an assistant message by its stable
// transcript identity.
func MaterializedAgentMessageText(items []TranscriptItem, stableID string) (string, bool) {
	for i := range items {
		if items[i].StableID != stableID {
			continue
		}
		if agent, ok := items[i].Body.(TranscriptBodyAgent); ok {
			return MaterializedChunksText(agent.Chunks), true
		}
		return "", false
	}
	return "", false
}

func MaterializedChunksText(chunks []json.RawMessage) string {
	panic("unported: mj-core/src/transcript.rs materialized_chunks_text")
}

func materializedValueText(value json.RawMessage) string {
	panic("unported: mj-core/src/transcript.rs materialized_value_text")
}

// ToolStatusOf maps an ACP tool call status. (Rust tool_status; renamed
// because ToolStatus is the type.)
func ToolStatusOf(status acp.ToolCallStatus) ToolStatus {
	switch status {
	case acp.ToolCallStatusInProgress:
		return ToolStatusRunning
	case acp.ToolCallStatusCompleted:
		return ToolStatusCompleted
	case acp.ToolCallStatusFailed:
		return ToolStatusFailed
	}
	return ToolStatusPending
}

func ContentBlockText(content *acp.ContentBlock) (string, bool) {
	panic("unported: mj-core/src/transcript.rs content_block_text")
}

// TruncateStringStart keeps the last maximumBytes bytes of value, moving
// the cut forward to a character boundary. It reports whether it cut.
func TruncateStringStart(value *string, maximumBytes int) bool {
	if len(*value) <= maximumBytes {
		return false
	}
	start := len(*value) - maximumBytes
	for start < len(*value) && !transcriptIsCharBoundary(*value, start) {
		start++
	}
	*value = (*value)[start:]
	return true
}

func transcriptIsCharBoundary(s string, i int) bool {
	return i == 0 || i == len(s) || (s[i]&0xC0) != 0x80
}

// ToolStatus is the ACP tool states needed to keep a compact tool block
// visually useful. It serializes with the Rust variant names.
type ToolStatus string

const (
	ToolStatusPending   ToolStatus = "Pending"
	ToolStatusRunning   ToolStatus = "Running"
	ToolStatusCompleted ToolStatus = "Completed"
	ToolStatusFailed    ToolStatus = "Failed"
)

// PlanStatus serializes with the Rust variant names.
type PlanStatus string

const (
	PlanStatusPending   PlanStatus = "Pending"
	PlanStatusRunning   PlanStatus = "Running"
	PlanStatusCompleted PlanStatus = "Completed"
)

type PlanLine struct {
	Text   string     `json:"text"`
	Status PlanStatus `json:"status"`
}

func (l *PlanLine) UnmarshalJSON(data []byte) error {
	type plain PlanLine
	return transcriptStrictDecode(data, (*plain)(l), false)
}

// ---------------------------------------------------------------------------
// serde emulation shared by the session modules (transcript, archive,
// attachment, mailbox, elicitation, history, subagent, usage).
//
// Struct tags drive it: a field is required unless it is a pointer, has
// omitempty, or carries `serde:"default"` (serde's #[serde(default)] without
// skip_serializing_if). Anonymous embedded structs are flattened.

// transcriptMarshal encodes v as serde_json does: compact, without Go's HTML
// escaping of <, > and &.
func transcriptMarshal(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// transcriptMarshalTagged encodes fields as an object that starts with
// tagKey: tag, as serde's internally tagged enums write a variant.
func transcriptMarshalTagged(tagKey, tag string, fields any) ([]byte, error) {
	key, err := transcriptMarshal(tagKey)
	if err != nil {
		return nil, err
	}
	value, err := transcriptMarshal(tag)
	if err != nil {
		return nil, err
	}
	out := append([]byte{'{'}, key...)
	out = append(out, ':')
	out = append(out, value...)
	if fields != nil {
		body, err := transcriptMarshal(fields)
		if err != nil {
			return nil, err
		}
		if len(body) > 2 {
			out = append(out, ',')
			out = append(out, body[1:len(body)-1]...)
		}
	}
	return append(out, '}'), nil
}

// transcriptMarshalAdjacent encodes an adjacently tagged variant
// ({tagKey: tag, contentKey: content}); a nil content is a unit variant,
// which serde writes with the tag alone.
func transcriptMarshalAdjacent(tagKey, tag, contentKey string, content any) ([]byte, error) {
	members := []transcriptMember{{tagKey, tag}}
	if content != nil {
		members = append(members, transcriptMember{contentKey, content})
	}
	return transcriptMarshalMembers(members)
}

type transcriptMember struct {
	key   string
	value any
}

// transcriptMarshalMembers encodes members as one object, in order.
func transcriptMarshalMembers(members []transcriptMember) ([]byte, error) {
	out := []byte{'{'}
	for i, member := range members {
		if i > 0 {
			out = append(out, ',')
		}
		key, err := transcriptMarshal(member.key)
		if err != nil {
			return nil, err
		}
		value, err := transcriptMarshal(member.value)
		if err != nil {
			return nil, err
		}
		out = append(out, key...)
		out = append(out, ':')
		out = append(out, value...)
	}
	return append(out, '}'), nil
}

// transcriptObject splits a JSON object into its members.
func transcriptObject(data []byte) (map[string]json.RawMessage, error) {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return nil, err
	}
	if members == nil {
		return nil, errors.New("invalid type: null, expected a map")
	}
	return members, nil
}

// transcriptTag reads the string tag member of an internally or adjacently
// tagged enum.
func transcriptTag(data []byte, tagKey, what string) (string, error) {
	members, err := transcriptObject(data)
	if err != nil {
		return "", err
	}
	raw, ok := members[tagKey]
	if !ok {
		return "", fmt.Errorf("missing field `%s`", tagKey)
	}
	var tag string
	if err := json.Unmarshal(raw, &tag); err != nil {
		return "", fmt.Errorf("invalid %s tag: %w", what, err)
	}
	return tag, nil
}

func transcriptUnknownVariant(tag string, expected ...string) error {
	return fmt.Errorf("unknown variant `%s`, %s", tag, transcriptExpected(expected))
}

// transcriptExpected words a list of names as serde's error messages do.
func transcriptExpected(names []string) string {
	quoted := make([]string, len(names))
	for i, name := range names {
		quoted[i] = "`" + name + "`"
	}
	switch len(quoted) {
	case 0:
		return "there are no fields"
	case 1:
		return "expected " + quoted[0]
	case 2:
		return "expected " + quoted[0] + " or " + quoted[1]
	}
	return "expected one of " + strings.Join(quoted, ", ")
}

// transcriptFields lists a struct type's JSON member names and the required
// ones, following the struct-tag rules above.
func transcriptFields(t reflect.Type) (known, required []string) {
	for t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	for i := 0; i < t.NumField(); i++ {
		field := t.Field(i)
		tag := field.Tag.Get("json")
		if tag == "-" {
			continue
		}
		name, options, _ := strings.Cut(tag, ",")
		if field.Anonymous && name == "" {
			k, r := transcriptFields(field.Type)
			known = append(known, k...)
			required = append(required, r...)
			continue
		}
		if !field.IsExported() {
			continue
		}
		if name == "" {
			name = field.Name
		}
		known = append(known, name)
		optional := field.Type.Kind() == reflect.Pointer ||
			strings.Contains(options, "omitempty") ||
			field.Tag.Get("serde") == "default"
		if !optional {
			required = append(required, name)
		}
	}
	return known, required
}

// transcriptStrictDecode decodes data into v (a pointer to a struct without
// its own UnmarshalJSON), rejecting a missing required member, and with deny
// any member that is neither a field of v nor one of extraKnown (the tag
// members of an enum).
func transcriptStrictDecode(data []byte, v any, deny bool, extraKnown ...string) error {
	members, err := transcriptObject(data)
	if err != nil {
		return err
	}
	known, required := transcriptFields(reflect.TypeOf(v))
	for _, name := range required {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	if deny {
		allowed := map[string]bool{}
		for _, name := range append(known, extraKnown...) {
			allowed[name] = true
		}
		names := make([]string, 0, len(members))
		for name := range members {
			names = append(names, name)
		}
		sort.Strings(names)
		for _, name := range names {
			if !allowed[name] {
				return fmt.Errorf("unknown field `%s`, %s", name, transcriptExpected(known))
			}
		}
	}
	return json.Unmarshal(data, v)
}

// transcriptNormalizeValue parses any JSON value and writes it back as
// serde_json::Value would (compact, preserve_order).
func transcriptNormalizeValue(data []byte) (json.RawMessage, error) {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return nil, err
	}
	return jsonvalue.Marshal(value)
}
