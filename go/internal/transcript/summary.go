// Port of mj-transcript/src/summary.rs.
//
// A deterministic model-facing view of history. Source transcripts stay
// lossless.
//
// Live updates arrive as relay observations, which carry acp-go's stable
// schema.SessionUpdate; stored calls are decoded into the unstable
// acp.ToolCall that MaterializedToolCallPresentation takes.

package transcript

import (
	"encoding/json"
	"fmt"
	"strings"
	"unicode"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// HandoffPreamble is the opening sentence of every handoff this module
// writes. Generation and detection share it so a later resume can always
// recognize its own prior handoff turns.
const HandoffPreamble = "You are continuing a coding session previously run by another ACP harness."

// ArchiveHandoffPreamble is the opening sentence of a hand-off written when
// an archived session is restored from the SessionWiki index. The restored
// session has no workspace from the old one, so it is marked apart from a
// cross-harness resume.
const ArchiveHandoffPreamble = "Archived session restored from SessionWiki."

// LegacyHandoffPreamble is the opening sentence of the byte-truncating
// handoff this pipeline replaced. Sessions resumed by that build still carry
// it in their transcripts.
const LegacyHandoffPreamble = "Continue this coding session from the portable transcript below."

// HandoffPlaceholder is what a prior handoff turn contributes to a new
// compaction. The transcript already carries the pre-resume lineage as
// ordinary turns, so repeating the handoff body would only spend budget on a
// summary of a summary.
const HandoffPlaceholder = "[cross-harness resume handoff: continuing work from a prior harness]"

// userText: prior handoffs repeat history already represented in the
// lineage.
func userText(text string) string {
	trimmed := strings.TrimLeftFunc(text, unicode.IsSpace)
	for _, preamble := range []string{HandoffPreamble, ArchiveHandoffPreamble, LegacyHandoffPreamble} {
		if strings.HasPrefix(trimmed, preamble) {
			return HandoffPlaceholder
		}
	}
	return text
}

const FullToolCalls = 8

// SummaryVersion: change when derived indexing content changes, even without
// a source update.
const SummaryVersion uint32 = 3

const DefaultSummaryBytes = 256 * 1024

const liveItems = 512
const liveItemBytes = 64 * 1024

type SummaryRole string

const (
	SummaryRoleUser      SummaryRole = "User"
	SummaryRoleAssistant SummaryRole = "Assistant"
	SummaryRoleMessage   SummaryRole = "Message"
	SummaryRoleTool      SummaryRole = "Tool"
	SummaryRolePlan      SummaryRole = "Plan"
)

type SummaryEntry struct {
	ID          string
	Position    uint64
	CreatedAtMs int64
	Role        SummaryRole
	Text        string
	// Tool is the full retained call or a compact, outcome-only object. Never
	// a presentation sidecar. nil is none (Rust's Option<Value>, which here
	// always holds an object).
	Tool         jsonvalue.Object
	terminalRefs []string
}

func (e *SummaryEntry) Body() string {
	if e.Tool == nil {
		return e.Text
	}
	encoded, err := jsonvalue.Marshal(e.Tool)
	if err != nil {
		panic(fmt.Sprintf("serialize a summary tool value: %v", err))
	}
	return string(encoded)
}

func (e *SummaryEntry) render(bodyLimit int) string {
	role := strings.ToLower(string(e.Role))
	body := e.Body()
	// Names and outcomes are outside the potentially large raw-body excerpt.
	label := ""
	if e.Role == SummaryRoleTool {
		label = " " + e.Text
	}
	return fmt.Sprintf("<%s%s>\n%s\n</%s>\n", role, label, Excerpt(body, bodyLimit), role)
}

// TranscriptSummary is usable as its zero value (Rust's Default).
type TranscriptSummary struct {
	Entries        []SummaryEntry
	nextPosition   uint64
	openMessage    *string
	leadingOmitted bool
}

// Excerpt is a head/tail excerpt with an explicit byte omission count, never
// broken UTF-8.
func Excerpt(text string, limit int) string {
	panic("unported: mj-transcript/src/summary.rs excerpt")
}

func toolValue(call json.RawMessage, presentation *core.ToolCallPresentation, full bool) jsonvalue.Object {
	panic("unported: mj-transcript/src/summary.rs tool_value")
}

// IndexedToolCall reconstructs indexed summary content without treating
// serialized detail as a tool name. Legacy indexes contain only titles and
// have no recoverable tool payload.
func IndexedToolCall(text, fallbackID string) (json.RawMessage, []core.CanonicalTerminalOutput) {
	panic("unported: mj-transcript/src/summary.rs indexed_tool_call")
}

func toolLabel(value jsonvalue.Object) string {
	panic("unported: mj-transcript/src/summary.rs tool_label")
}

func TranscriptSummaryFromSnapshot(snapshot *core.CanonicalSessionSnapshot) TranscriptSummary {
	return TranscriptSummaryFromSnapshotKeeping(snapshot, FullToolCalls)
}

// TranscriptSummaryFromSnapshotKeeping is like TranscriptSummaryFromSnapshot,
// keeping the full call for the last fullToolCalls tool calls instead of
// FullToolCalls. Rust's usize::MAX is math.MaxInt.
func TranscriptSummaryFromSnapshotKeeping(snapshot *core.CanonicalSessionSnapshot, fullToolCalls int) TranscriptSummary {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::from_snapshot_keeping")
}

func TranscriptSummaryFromMaterialized(session *core.MaterializedSession) TranscriptSummary {
	return TranscriptSummaryFromMaterializedKeeping(session, FullToolCalls)
}

// TranscriptSummaryFromMaterializedKeeping is like
// TranscriptSummaryFromMaterialized, keeping the full call for the last
// fullToolCalls tool calls instead of FullToolCalls.
func TranscriptSummaryFromMaterializedKeeping(session *core.MaterializedSession, fullToolCalls int) TranscriptSummary {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::from_materialized_keeping")
}

func (s *TranscriptSummary) MarkEarlierHistoryOmitted() {
	s.leadingOmitted = true
}

func (s *TranscriptSummary) PushUser(text string) {
	s.openMessage = nil
	s.push(SummaryRoleUser, "", userText(text), nil)
}

func (s *TranscriptSummary) push(role SummaryRole, id, text string, tool jsonvalue.Object) {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::push")
}

func (s *TranscriptSummary) Observe(update *schema.SessionUpdate) {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::observe")
}

func (s *TranscriptSummary) ObserveTerminal(terminal *core.TerminalOutputRecord) {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::observe_terminal")
}

func (s *TranscriptSummary) demoteTools() {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::demote_tools")
}

// LatestUserMessages is Jev's current-request conversation, without
// historical tool-call records. Keep the available suffix and its coverage
// marker if replay lacks the user boundary.
func (s *TranscriptSummary) LatestUserMessages() TranscriptSummary {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::latest_user_messages")
}

// Retained is the exact retained context alongside a model-produced summary,
// newest-first selection.
func (s *TranscriptSummary) Retained() TranscriptSummary {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::retained")
}

// Render preserves recent context first, then spends spare space on newest
// history. Only after optional history is removed do oversized retained
// bodies shrink.
func (s *TranscriptSummary) Render(limit int) string {
	text, _ := s.RenderWithIDs(limit)
	return text
}

// RenderWithIDs returns the retained entry identities as well as text, so
// offline evidence selectors never ask about entries omitted by the
// rendering budget.
func (s *TranscriptSummary) RenderWithIDs(limit int) (string, []string) {
	panic("unported: mj-transcript/src/summary.rs TranscriptSummary::render_with_ids")
}

// attachedTerminal is one terminal attach_terminals records on a tool value
// (Rust's tuple of id, output, exit code, signal and truncation).
type attachedTerminal struct {
	id        string
	output    string
	code      *uint32
	signal    *string
	truncated bool
}

func attachTerminals(value *jsonvalue.Object, terminals []attachedTerminal) {
	panic("unported: mj-transcript/src/summary.rs attach_terminals")
}

// boundValue excerpts every string longer than limit in a jsonvalue value,
// in place for arrays and objects, and returns the value.
func boundValue(value any, limit int) any {
	switch v := value.(type) {
	case string:
		if len(v) > limit {
			return Excerpt(v, limit)
		}
	case []any:
		for i := range v {
			v[i] = boundValue(v[i], limit)
		}
	case jsonvalue.Object:
		for i := range v {
			v[i].Value = boundValue(v[i].Value, limit)
		}
	}
	return value
}
