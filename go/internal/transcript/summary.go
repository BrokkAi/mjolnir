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
	"log/slog"
	"math"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

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

// UserText: prior handoffs repeat history already represented in the
// lineage.
func UserText(text string) string {
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
	if len(text) <= limit {
		return text
	}
	markerBudget := len(fmt.Sprintf("\n[%d bytes omitted]\n", len(text)))
	if limit < markerBudget {
		return "[omitted]"[:min(limit, len("[omitted]"))]
	}
	available := limit - markerBudget
	head := available / 2
	for !summaryIsCharBoundary(text, head) {
		head--
	}
	tail := len(text) - (available - available/2)
	for !summaryIsCharBoundary(text, tail) {
		tail++
	}
	return fmt.Sprintf("%s\n[%d bytes omitted]\n%s", text[:head], tail-head, text[tail:])
}

// summaryIsCharBoundary is Rust's str::is_char_boundary.
func summaryIsCharBoundary(text string, index int) bool {
	return index == 0 || index == len(text) || (index < len(text) && utf8.RuneStart(text[index]))
}

// ToolOutcome is how a tool call ended, as its raw output and terminals
// report it.
type ToolOutcome struct {
	ExitCode *int64
	Signal   *string
}

// ToolOutcomeFromRawOutput reads the outcome a raw output reports. output is
// the raw output as a jsonvalue value, nil when the call has none.
func ToolOutcomeFromRawOutput(output any) ToolOutcome {
	var outcome ToolOutcome
	if code, ok := jsonAsI64(jsonMember(output, "exit_code")); ok {
		outcome.ExitCode = &code
	}
	if signal, ok := transcriptGetString(output, "signal"); ok {
		outcome.Signal = &signal
	}
	return outcome
}

// ObserveTerminal records what one terminal reports; a terminal that says
// nothing leaves the earlier report.
func (o *ToolOutcome) ObserveTerminal(code *uint32, signal *string) {
	if code != nil {
		exitCode := int64(*code)
		o.ExitCode = &exitCode
	}
	if signal != nil {
		o.Signal = signal
	}
}

func (o *ToolOutcome) IsError() bool {
	return o.Signal != nil || (o.ExitCode != nil && *o.ExitCode != 0)
}

func (o *ToolOutcome) writeTo(value *jsonvalue.Object) {
	if o.ExitCode != nil {
		value.Set("exit_code", *o.ExitCode)
	}
	if o.Signal != nil {
		value.Set("signal", *o.Signal)
	}
}

func toolValue(call json.RawMessage, presentation *core.ToolCallPresentation, full bool) jsonvalue.Object {
	var name string
	var decoded acp.ToolCall
	if err := json.Unmarshal(call, &decoded); err != nil {
		slog.Warn("could not decode tool for transcript summary", "error", err)
		name = "[invalid tool call]"
	} else {
		name = MaterializedToolCallPresentation(presentation, &decoded).Summary
	}
	parsed, _ := jsonvalue.Parse(call)
	member := func(key, fallback string) string {
		if text, ok := transcriptGetString(parsed, key); ok {
			return text
		}
		return fallback
	}
	compact := jsonvalue.Object{
		{Key: "toolCallId", Value: member("toolCallId", "unknown")},
		{Key: "name", Value: core.ToolTitle(name)},
		{Key: "kind", Value: member("kind", "other")},
		{Key: "status", Value: member("status", "pending")},
	}
	outcome := ToolOutcomeFromRawOutput(jsonMember(parsed, "rawOutput"))
	outcome.writeTo(&compact)
	if full {
		compact.Set("call", parsed)
	}
	return compact
}

// IndexedToolCall reconstructs indexed summary content without treating
// serialized detail as a tool name. Legacy indexes contain only titles and
// have no recoverable tool payload.
func IndexedToolCall(text, fallbackID string) (json.RawMessage, []core.CanonicalTerminalOutput) {
	if parsed, err := jsonvalue.Parse([]byte(text)); err == nil {
		_, nameOK := transcriptGetString(parsed, "name")
		_, statusOK := transcriptGetString(parsed, "status")
		_, idOK := transcriptGetString(parsed, "toolCallId")
		if nameOK && statusOK && idOK {
			value := parsed.(jsonvalue.Object)
			if call, ok := value.Get("call"); ok {
				encoded := mustMarshalJSONValue(call)
				var decoded schema.ToolCall
				if json.Unmarshal(encoded, &decoded) == nil {
					terminals := []core.CanonicalTerminalOutput{}
					if records, ok := jsonMember(value, "terminals").(jsonvalue.Object); ok {
						for _, member := range records {
							record, ok := member.Value.(jsonvalue.Object)
							if !ok {
								slog.Warn("invalid indexed terminal summary")
								continue
							}
							record = record.Clone()
							record.Set("terminal_id", member.Key)
							var output core.CanonicalTerminalOutput
							if err := json.Unmarshal(mustMarshalJSONValue(record), &output); err != nil {
								slog.Warn("invalid indexed terminal summary", "error", err)
								continue
							}
							terminals = append(terminals, output)
						}
					}
					return encoded, terminals
				}
			}
			kind, ok := value.Get("kind")
			if !ok {
				kind = "other"
			}
			id, _ := value.Get("toolCallId")
			name, _ := value.Get("name")
			status, _ := value.Get("status")
			call := jsonvalue.Object{
				{Key: "toolCallId", Value: id},
				{Key: "title", Value: name},
				{Key: "status", Value: status},
				{Key: "kind", Value: kind},
			}
			for _, key := range []string{"exit_code", "signal"} {
				outcome, ok := value.Get(key)
				if !ok {
					continue
				}
				rawOutput, isObject := jsonMember(call, "rawOutput").(jsonvalue.Object)
				if !isObject {
					rawOutput = jsonvalue.Object{}
				}
				rawOutput.Set(key, outcome)
				call.Set("rawOutput", rawOutput)
			}
			return mustMarshalJSONValue(call), []core.CanonicalTerminalOutput{}
		}
	}
	return mustMarshalJSONValue(jsonvalue.Object{
		{Key: "toolCallId", Value: fallbackID},
		{Key: "title", Value: text},
		{Key: "status", Value: "completed"},
	}), []core.CanonicalTerminalOutput{}
}

// mustMarshalJSONValue writes a jsonvalue value as serde_json does; these
// values are always serializable.
func mustMarshalJSONValue(value any) json.RawMessage {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		panic(fmt.Sprintf("serialize a JSON value: %v", err))
	}
	return encoded
}

func toolLabel(value jsonvalue.Object) string {
	name, ok := transcriptGetString(value, "name")
	if !ok {
		name = "tool"
	}
	status, ok := transcriptGetString(value, "status")
	if !ok {
		status = "unknown"
	}
	label := fmt.Sprintf("%s [%s]", name, status)
	if code, ok := value.Get("exit_code"); ok {
		label += " exit=" + string(mustMarshalJSONValue(code))
	}
	if signal, ok := transcriptGetString(value, "signal"); ok {
		label += " signal=" + signal
	}
	return label
}

func TranscriptSummaryFromSnapshot(snapshot *core.CanonicalSessionSnapshot) TranscriptSummary {
	return TranscriptSummaryFromSnapshotKeeping(snapshot, FullToolCalls)
}

// TranscriptSummaryFromSnapshotKeeping is like TranscriptSummaryFromSnapshot,
// keeping the full call for the last fullToolCalls tool calls instead of
// FullToolCalls. Rust's usize::MAX is math.MaxInt.
func TranscriptSummaryFromSnapshotKeeping(snapshot *core.CanonicalSessionSnapshot, fullToolCalls int) TranscriptSummary {
	var summary TranscriptSummary
	outputs := core.TerminalOutputsOfCanonical(snapshot.Transcript)
	start := snapshot.CurrentContextStart()
	inContext := func(item *core.CanonicalTranscriptItem) bool {
		return start == 0 || item.Position > start
	}
	cutoff := uint64(math.MaxUint64)
	kept := 0
	for i := len(snapshot.Transcript) - 1; i >= 0 && kept < fullToolCalls; i-- {
		item := &snapshot.Transcript[i]
		if _, ok := item.Body.(core.CanonicalTranscriptBodyTool); ok && inContext(item) {
			cutoff = min(cutoff, item.Position)
			kept++
		}
	}
	for i := range snapshot.Transcript {
		item := &snapshot.Transcript[i]
		if !inContext(item) {
			continue
		}
		var role SummaryRole
		var text string
		var tool jsonvalue.Object
		switch body := item.Body.(type) {
		case core.CanonicalTranscriptBodyUser:
			role, text = SummaryRoleUser, UserText(core.MaterializedContentText(body.Content))
		case core.CanonicalTranscriptBodyMessage:
			role, text = SummaryRoleMessage, core.RenderMailboxEvent(body.Event)
		case core.CanonicalTranscriptBodyAgent:
			role, text = SummaryRoleAssistant, core.MaterializedChunksText(body.Chunks)
		case core.CanonicalTranscriptBodyTool:
			tool = toolValue(body.Call, body.Presentation, item.Position >= cutoff)
			var attached []attachedTerminal
			for _, terminal := range outputs.ForTool(item.StableID) {
				attached = append(attached, attachedTerminal{
					id: terminal.TerminalID, output: terminal.Output, code: terminal.ExitCode,
					signal: terminal.Signal, truncated: terminal.Truncated,
				})
			}
			attachTerminals(&tool, attached)
			role, text = SummaryRoleTool, toolLabel(tool)
		case core.CanonicalTranscriptBodyPlan:
			role, text = SummaryRolePlan, compactJSON(body.Plan)
		default:
			continue
		}
		summary.Entries = append(summary.Entries, SummaryEntry{
			ID:           item.StableID,
			Position:     item.Position,
			CreatedAtMs:  item.CreatedAtMs,
			Role:         role,
			Text:         text,
			terminalRefs: toolTerminalRefs(tool),
			Tool:         tool,
		})
	}
	summary.nextPosition = saturatingIncrement(snapshot.EventFrontier)
	return summary
}

// toolTerminalRefs is the terminals a tool value's retained call refers to;
// none when it keeps no call.
func toolTerminalRefs(tool jsonvalue.Object) []string {
	call, ok := tool.Get("call")
	if !ok {
		return []string{}
	}
	refs := toolCallTerminalIDs(mustMarshalJSONValue(call))
	if refs == nil {
		return []string{}
	}
	return refs
}

// compactJSON is serde_json's Display of a stored value: compact, members in
// order.
func compactJSON(raw json.RawMessage) string {
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return string(raw)
	}
	return string(mustMarshalJSONValue(value))
}

// saturatingIncrement is Rust's u64::saturating_add(1).
func saturatingIncrement(value uint64) uint64 {
	if value == math.MaxUint64 {
		return value
	}
	return value + 1
}

func TranscriptSummaryFromMaterialized(session *core.MaterializedSession) TranscriptSummary {
	return TranscriptSummaryFromMaterializedKeeping(session, FullToolCalls)
}

// TranscriptSummaryFromMaterializedKeeping is like
// TranscriptSummaryFromMaterialized, keeping the full call for the last
// fullToolCalls tool calls instead of FullToolCalls.
func TranscriptSummaryFromMaterializedKeeping(session *core.MaterializedSession, fullToolCalls int) TranscriptSummary {
	var start uint64
	for _, item := range session.Transcript {
		if core.IsContextBoundary(item.StableID) {
			start = max(start, item.Position)
		}
	}
	inContext := func(item *core.TranscriptItem) bool {
		return start == 0 || item.Position > start
	}
	cutoff := uint64(math.MaxUint64)
	kept := 0
	for i := len(session.Transcript) - 1; i >= 0 && kept < fullToolCalls; i-- {
		item := session.Transcript[i]
		if _, ok := item.Body.(core.TranscriptBodyTool); ok && inContext(item) {
			cutoff = min(cutoff, item.Position)
			kept++
		}
	}
	items := make([]core.TranscriptItem, len(session.Transcript))
	for i, item := range session.Transcript {
		items[i] = *item
	}
	outputs := core.TerminalOutputsOf(items)
	var summary TranscriptSummary
	for _, item := range session.Transcript {
		if !inContext(item) {
			continue
		}
		var role SummaryRole
		var text string
		var tool jsonvalue.Object
		switch body := item.Body.(type) {
		case core.TranscriptBodyUser:
			role, text = SummaryRoleUser, UserText(core.MaterializedContentText(body.Content))
		case core.TranscriptBodyMessage:
			role, text = SummaryRoleMessage, core.RenderMailboxEvent(body.Event)
		case core.TranscriptBodyAgent:
			role, text = SummaryRoleAssistant, core.MaterializedChunksText(body.Chunks)
		case core.TranscriptBodyTool:
			tool = toolValue(body.Call.Value(), body.Presentation, item.Position >= cutoff)
			var attached []attachedTerminal
			for _, terminal := range outputs.ForTool(item.StableID) {
				attached = append(attached, attachedTerminal{
					id: terminal.TerminalID, output: terminal.Output.Text(), code: terminal.ExitCode,
					signal: terminal.Signal, truncated: terminal.Truncated,
				})
			}
			attachTerminals(&tool, attached)
			role, text = SummaryRoleTool, toolLabel(tool)
		case core.TranscriptBodyPlan:
			role, text = SummaryRolePlan, compactJSON(body.Plan)
		default:
			continue
		}
		summary.Entries = append(summary.Entries, SummaryEntry{
			ID:           item.StableID,
			Position:     item.Position,
			CreatedAtMs:  item.CreatedAtMs,
			Role:         role,
			Text:         text,
			terminalRefs: toolTerminalRefs(tool),
			Tool:         tool,
		})
	}
	summary.nextPosition = saturatingIncrement(session.AppliedEventOrdinal)
	return summary
}

func (s *TranscriptSummary) MarkEarlierHistoryOmitted() {
	s.leadingOmitted = true
}

func (s *TranscriptSummary) PushUser(text string) {
	s.openMessage = nil
	s.push(SummaryRoleUser, "", UserText(text), nil)
}

func (s *TranscriptSummary) push(role SummaryRole, id, text string, tool jsonvalue.Object) {
	s.nextPosition = saturatingIncrement(s.nextPosition)
	s.Entries = append(s.Entries, SummaryEntry{
		ID:           id,
		Position:     s.nextPosition,
		CreatedAtMs:  0,
		Role:         role,
		Text:         Excerpt(text, liveItemBytes),
		terminalRefs: toolTerminalRefs(tool),
		Tool:         tool,
	})
	if len(s.Entries) > liveItems {
		var fullTools []int
		for i := len(s.Entries) - 1; i >= 0 && len(fullTools) < FullToolCalls; i-- {
			if s.Entries[i].Role == SummaryRoleTool {
				fullTools = append(fullTools, i)
			}
		}
		latestUser := s.latestIndex(SummaryRoleUser)
		remove := 0
		for i := range s.Entries {
			if !slices.Contains(fullTools, i) && i != latestUser {
				remove = i
				break
			}
		}
		s.Entries = slices.Delete(s.Entries, remove, remove+1)
		s.leadingOmitted = true
	}
}

// latestIndex is the index of the last entry with role, -1 when none.
func (s *TranscriptSummary) latestIndex(role SummaryRole) int {
	for i := len(s.Entries) - 1; i >= 0; i-- {
		if s.Entries[i].Role == role {
			return i
		}
	}
	return -1
}

// toolEntry is the tool entry for a call, nil when none.
func (s *TranscriptSummary) toolEntry(id string) *SummaryEntry {
	for i := range s.Entries {
		if s.Entries[i].Role == SummaryRoleTool && s.Entries[i].ID == id {
			return &s.Entries[i]
		}
	}
	return nil
}

// addTerminalRefs records the terminals a tool value's call refers to.
func (e *SummaryEntry) addTerminalRefs(value jsonvalue.Object) {
	for _, id := range toolTerminalRefs(value) {
		if !slices.Contains(e.terminalRefs, id) {
			e.terminalRefs = append(e.terminalRefs, id)
		}
	}
}

// encodeJSONValue is serde_json::to_value of an ACP value.
func encodeJSONValue(value any) any {
	encoded, err := json.Marshal(value)
	if err != nil {
		panic(fmt.Sprintf("serialize ACP value: %v", err))
	}
	parsed, err := jsonvalue.Parse(encoded)
	if err != nil {
		panic(fmt.Sprintf("serialize ACP value: %v", err))
	}
	return parsed
}

func (s *TranscriptSummary) Observe(update *schema.SessionUpdate) {
	switch {
	case update.AgentMessageChunk != nil:
		chunk := update.AgentMessageChunk
		id := ""
		if chunk.MessageID != nil {
			id = string(*chunk.MessageID)
		}
		if chunk.Content.Text != nil {
			text := chunk.Content.Text.Text
			if s.openMessage != nil && *s.openMessage == id {
				if len(s.Entries) > 0 {
					entry := &s.Entries[len(s.Entries)-1]
					entry.Text += Excerpt(text, liveItemBytes)
					if len(entry.Text) > liveItemBytes {
						entry.Text = Excerpt(entry.Text, liveItemBytes)
					}
				}
			} else {
				s.push(SummaryRoleAssistant, id, Excerpt(text, liveItemBytes), nil)
			}
			s.openMessage = &id
		}
	case update.ToolCall != nil:
		s.openMessage = nil
		call := update.ToolCall
		id := string(call.ToolCallID)
		raw := mustMarshalJSONValue(encodeJSONValue(call))
		full := true
		for i := range s.Entries {
			if s.Entries[i].ID == id {
				_, full = s.Entries[i].Tool.Get("call")
				break
			}
		}
		value := toolValue(raw, nil, full)
		if entry := s.toolEntry(id); entry != nil {
			entry.addTerminalRefs(value)
			entry.Text = toolLabel(value)
			entry.Tool = value
		} else {
			s.push(SummaryRoleTool, id, toolLabel(value), value)
		}
		s.demoteTools()
	case update.ToolCallUpdate != nil:
		s.openMessage = nil
		fields := update.ToolCallUpdate
		entry := s.toolEntry(string(fields.ToolCallID))
		if entry == nil {
			break
		}
		old := entry.Tool
		if old == nil {
			panic("tool entry")
		}
		oldCall, full := old.Get("call")
		var call schema.ToolCall
		decoded := false
		if full {
			decoded = json.Unmarshal(mustMarshalJSONValue(oldCall), &call) == nil
		}
		if !decoded {
			name, ok := transcriptGetString(old, "name")
			if !ok {
				name = "tool"
			}
			call = schema.ToolCall{ToolCallID: fields.ToolCallID, Title: name}
		}
		previousName, _ := old.Get("name")
		previousStatus, _ := old.Get("status")
		updateSchemaToolCall(&call, fields)
		raw := mustMarshalJSONValue(encodeJSONValue(call))
		value := toolValue(raw, nil, full)
		if !full && fields.RawInput == nil {
			value.Set("name", previousName)
		}
		if fields.Kind == nil {
			kind, _ := old.Get("kind")
			value.Set("kind", kind)
		}
		if fields.Status == nil {
			value.Set("status", previousStatus)
		}
		for _, key := range []string{"exit_code", "signal", "terminals"} {
			if _, ok := value.Get(key); ok {
				continue
			}
			if oldValue, ok := old.Get(key); ok {
				value.Set(key, oldValue)
			}
		}
		entry.addTerminalRefs(value)
		entry.Text = toolLabel(value)
		entry.Tool = value
	case update.AgentThoughtChunk != nil:
		s.openMessage = nil
	case update.Plan != nil:
		s.openMessage = nil
		s.push(SummaryRolePlan, "", string(mustMarshalJSONValue(encodeJSONValue(update.Plan))), nil)
	}
	// Bound retained live payloads, including strings nested in raw ACP
	// records.
	if update.ToolCall != nil || update.ToolCallUpdate != nil {
		for i := range s.Entries {
			if s.Entries[i].Tool != nil {
				boundValue(s.Entries[i].Tool, liveItemBytes)
			}
		}
	}
}

// updateSchemaToolCall is the ACP crate's ToolCall::update on the stable
// schema the relay observations carry: each field present in the update
// replaces the call's.
func updateSchemaToolCall(call *schema.ToolCall, fields *schema.ToolCallUpdate) {
	if fields.Title != nil {
		call.Title = *fields.Title
	}
	if fields.Name != nil {
		name := *fields.Name
		call.Name = &name
	}
	if fields.Kind != nil {
		kind := *fields.Kind
		call.Kind = &kind
	}
	if fields.Status != nil {
		status := *fields.Status
		call.Status = &status
	}
	if fields.Content != nil {
		call.Content = fields.Content
	}
	if fields.Locations != nil {
		call.Locations = fields.Locations
	}
	if fields.RawInput != nil {
		call.RawInput = fields.RawInput
	}
	if fields.RawOutput != nil {
		call.RawOutput = fields.RawOutput
	}
}

func (s *TranscriptSummary) ObserveTerminal(terminal *core.TerminalOutputRecord) {
	for i := range s.Entries {
		entry := &s.Entries[i]
		if entry.Tool == nil {
			continue
		}
		if slices.Contains(entry.terminalRefs, terminal.TerminalID) {
			attachTerminals(&entry.Tool, []attachedTerminal{{
				id: terminal.TerminalID, output: terminal.Output.Text(), code: terminal.ExitCode,
				signal: terminal.Signal, truncated: terminal.Truncated,
			}})
			boundValue(entry.Tool, liveItemBytes)
			entry.Text = toolLabel(entry.Tool)
		}
	}
}

func (s *TranscriptSummary) demoteTools() {
	skipped := 0
	for i := len(s.Entries) - 1; i >= 0; i-- {
		entry := &s.Entries[i]
		if entry.Role != SummaryRoleTool {
			continue
		}
		if skipped < FullToolCalls {
			skipped++
			continue
		}
		if entry.Tool != nil {
			// serde_json's Map::remove under preserve_order is swap_remove.
			entry.Tool.SwapDelete("call")
			entry.Tool.SwapDelete("terminals")
		}
	}
}

// clone is a deep copy of the entry, as Rust's Clone copies the value.
func (e SummaryEntry) clone() SummaryEntry {
	if e.Tool != nil {
		e.Tool = e.Tool.Clone()
	}
	e.terminalRefs = slices.Clone(e.terminalRefs)
	return e
}

// LatestUserMessages is Jev's current-request conversation, without
// historical tool-call records. Keep the available suffix and its coverage
// marker if replay lacks the user boundary.
func (s *TranscriptSummary) LatestUserMessages() TranscriptSummary {
	user := s.latestIndex(SummaryRoleUser)
	var entries []SummaryEntry
	for _, entry := range s.Entries[max(user, 0):] {
		if entry.Role != SummaryRoleTool {
			entries = append(entries, entry.clone())
		}
	}
	return TranscriptSummary{Entries: entries, leadingOmitted: s.leadingOmitted && user < 0}
}

// Retained is the exact retained context alongside a model-produced summary,
// newest-first selection.
func (s *TranscriptSummary) Retained() TranscriptSummary {
	lastAssistant := s.latestIndex(SummaryRoleAssistant)
	var entries []SummaryEntry
	for i, entry := range s.Entries {
		_, full := entry.Tool.Get("call")
		if entry.Role == SummaryRoleUser || entry.Role == SummaryRoleMessage || i == lastAssistant || full {
			entries = append(entries, entry.clone())
		}
	}
	return TranscriptSummary{Entries: entries}
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
	historyMarker := ""
	if s.leadingOmitted {
		historyMarker = "[Earlier transcript entries omitted]\n"
	}
	if limit < len(historyMarker) {
		return Excerpt(historyMarker, limit), []string{}
	}
	var all strings.Builder
	all.WriteString(historyMarker)
	for i := range s.Entries {
		all.WriteString(s.Entries[i].render(math.MaxInt))
	}
	if all.Len() <= limit {
		ids := make([]string, len(s.Entries))
		for i := range s.Entries {
			ids[i] = s.Entries[i].ID
		}
		return all.String(), ids
	}

	// selected is Rust's BTreeSet<usize>: membership here, order by sorting.
	selected := map[int]bool{}
	for _, role := range []SummaryRole{SummaryRoleUser, SummaryRoleMessage, SummaryRoleAssistant} {
		if index := s.latestIndex(role); index >= 0 {
			selected[index] = true
		}
	}
	tools := 0
	for i := len(s.Entries) - 1; i >= 0 && tools < FullToolCalls; i-- {
		if s.Entries[i].Role == SummaryRoleTool {
			selected[i] = true
			tools++
		}
	}
	used := 0
	for index := range selected {
		used += len(s.Entries[index].render(math.MaxInt))
	}
	// Reserve enough room for both coverage markers before adding optional
	// items.
	available := max(limit-(len(historyMarker)+64), 0)
	if used < available {
		// Recent requests and received messages take precedence over old
		// activity.
		for _, users := range []bool{true, false} {
			for index := len(s.Entries) - 1; index >= 0; index-- {
				entry := &s.Entries[index]
				isUser := entry.Role == SummaryRoleUser || entry.Role == SummaryRoleMessage
				if selected[index] || isUser != users {
					continue
				}
				size := len(entry.render(math.MaxInt))
				if saturatingAdd(used, size) <= available {
					selected[index] = true
					used += size
				}
			}
		}
	}
	order := make([]int, 0, len(selected))
	for index := range selected {
		order = append(order, index)
	}
	slices.Sort(order)
	omitted := len(s.Entries) - len(order)
	prefix := historyMarker
	if omitted > 0 {
		prefix += fmt.Sprintf("[%d transcript entries omitted]\n", omitted)
	}
	render := func(bodyCap int) string {
		var text strings.Builder
		text.WriteString(prefix)
		for _, index := range order {
			text.WriteString(s.Entries[index].render(bodyCap))
		}
		return text.String()
	}
	ids := make([]string, len(order))
	for i, index := range order {
		ids[i] = s.Entries[index].ID
	}
	full := render(math.MaxInt)
	if len(full) <= limit {
		return full, ids
	}
	minimum := render(0)
	if len(minimum) > limit {
		return Excerpt(minimum, limit), []string{}
	}
	// A common cap leaves short messages intact and shares remaining space
	// among long bodies.
	low, high := 0, limit
	for low < high {
		middle := low + (high-low+1)/2
		if len(render(middle)) <= limit {
			low = middle
		} else {
			high = middle - 1
		}
	}
	return render(low), ids
}

// saturatingAdd is Rust's usize::saturating_add.
func saturatingAdd(a, b int) int {
	if a > math.MaxInt-b {
		return math.MaxInt
	}
	return a + b
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
	var outcome ToolOutcome
	for _, terminal := range terminals {
		outcome.ObserveTerminal(terminal.code, terminal.signal)
		var code, signal any
		if terminal.code != nil {
			code = *terminal.code
		}
		if terminal.signal != nil {
			signal = *terminal.signal
		}
		if _, ok := value.Get("call"); ok {
			records, ok := jsonMember(*value, "terminals").(jsonvalue.Object)
			if !ok {
				records = jsonvalue.Object{}
			}
			records.Set(terminal.id, jsonvalue.Object{
				{Key: "output", Value: terminal.output},
				{Key: "exit_code", Value: code},
				{Key: "signal", Value: signal},
				{Key: "truncated", Value: terminal.truncated},
			})
			value.Set("terminals", records)
		}
	}
	outcome.writeTo(value)
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
