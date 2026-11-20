// Port of mj-transcript/src/projection/terminals.rs.

package transcript

import (
	"encoding/json"
	"errors"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// applyTerminalMetadata folds the ACP v1 display-terminal extension into the
// same durable records used for client-owned terminals. Metadata on updates
// is not part of ToolCallUpdateFields, so applying the update alone loses
// these bytes. A nil meta is Rust's None.
func applyTerminalMetadata(item *core.TranscriptItem, meta jsonvalue.Object) {
	panic("unported: mj-transcript/src/projection/terminals.rs apply_terminal_metadata")
}

// terminalMetadataOutput is the terminal_output and terminal_output_delta
// metadata (Rust's local Output). Both fields are required.
type terminalMetadataOutput struct {
	TerminalID string `json:"terminal_id"`
	Data       string `json:"data"`
}

func (o *terminalMetadataOutput) UnmarshalJSON(data []byte) error {
	var v struct {
		TerminalID *string `json:"terminal_id"`
		Data       *string `json:"data"`
	}
	if err := json.Unmarshal(data, &v); err != nil {
		return err
	}
	if v.TerminalID == nil {
		return errors.New("missing field `terminal_id`")
	}
	if v.Data == nil {
		return errors.New("missing field `data`")
	}
	*o = terminalMetadataOutput{TerminalID: *v.TerminalID, Data: *v.Data}
	return nil
}

// terminalMetadataExit is the terminal_exit metadata (Rust's local Exit).
// TerminalID is required; the others default to nil.
type terminalMetadataExit struct {
	TerminalID string  `json:"terminal_id"`
	ExitCode   *uint32 `json:"exit_code"`
	Signal     *string `json:"signal"`
}

func (e *terminalMetadataExit) UnmarshalJSON(data []byte) error {
	type plain terminalMetadataExit
	var v struct {
		plain
		TerminalID *string `json:"terminal_id"`
	}
	if err := json.Unmarshal(data, &v); err != nil {
		return err
	}
	if v.TerminalID == nil {
		return errors.New("missing field `terminal_id`")
	}
	*e = terminalMetadataExit{TerminalID: *v.TerminalID, ExitCode: v.ExitCode, Signal: v.Signal}
	return nil
}

// terminalItemID is the stable id of the standalone item that holds a
// terminal's output until a tool call refers to it.
func terminalItemID(terminalID string) string {
	return "terminal:" + terminalID
}

// toolCallTerminalIDs is the terminal ids one stored ACP tool call refers
// to (Rust re-exports mj-core's tool_call_terminal_ids here for the crate).
// This is the only place that reads terminal content out of a call, so
// attaching output and consuming a parked item cannot disagree about what a
// call refers to.
//
// Content hel cannot read as an ACP block names no terminal; the renderer
// already reports such a call as invalid, so this hides no failure.
func toolCallTerminalIDs(call json.RawMessage) []string {
	return core.ToolCallTerminalIDs(call)
}

// findTerminalRecord is the output already recorded for terminalID,
// wherever it is parked.
func findTerminalRecord(index *ProjectionIndex, terminalID string) (core.TerminalOutputRecord, bool) {
	item := index.get(terminalItemID(terminalID))
	if item == nil {
		return core.TerminalOutputRecord{}, false
	}
	body, ok := item.Body.(core.TranscriptBodyTerminalOutput)
	if !ok || body.Record.TerminalID != terminalID {
		return core.TerminalOutputRecord{}, false
	}
	return body.Record, true
}

func replaceOrPushTerminalRecord(records *[]core.TerminalOutputRecord, record core.TerminalOutputRecord) {
	for i := range *records {
		if (*records)[i].TerminalID == record.TerminalID {
			(*records)[i] = record
			return
		}
	}
	*records = append(*records, record)
}

func fallbackTerminalToolItemID(terminalID string) string {
	return "tool:" + core.FallbackTerminalToolCallID(terminalID)
}

func fallbackToolItem(item *core.TranscriptItem) (bool, error) {
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return false, nil
	}
	if !body.Call.TypedValid() {
		return false, errors.New("invalid ACP fallback terminal tool call")
	}
	return body.Call.IsFallback(), nil
}

// uniquelyMatchingRawTool is the one provider tool demonstrably owning a
// result through its raw value. Identical concurrent results are
// deliberately left with the fallback tool.
func uniquelyMatchingRawTool(index *ProjectionIndex, record *core.TerminalOutputRecord) *core.TranscriptItem {
	panic("unported: mj-transcript/src/projection/terminals.rs uniquely_matching_raw_tool")
}

// fallbackTerminalAlreadyClaimed: a provider may publish its real tool
// before it asks the client to create the terminal. In that ordering the
// existing call already provides the durable start item, so the
// compatibility call would only duplicate it.
func fallbackTerminalAlreadyClaimed(index *ProjectionIndex, call *schema.ToolCall) (bool, error) {
	panic("unported: mj-transcript/src/projection/terminals.rs fallback_terminal_already_claimed")
}

// consumeFallbackTerminalTools replaces Hel's interim terminal tool with the
// real ACP tool once the agent supplies one. Output may already have landed
// on the interim item, so move it along. A newly created real item can also
// adopt the interim item's start identity; an existing tool keeps its
// immutable position and timestamp.
func consumeFallbackTerminalTools(index *ProjectionIndex, mutation *core.MaterializedSessionMutation, item *core.TranscriptItem, mayAdoptIdentity bool) error {
	panic("unported: mj-transcript/src/projection/terminals.rs consume_fallback_terminal_tools")
}

// finalizeFallbackTerminalTool: terminal close is a Hel event rather than an
// ACP tool update. Complete only the interim call Hel created; a
// provider-owned call keeps its own status.
func finalizeFallbackTerminalTool(item *core.TranscriptItem) error {
	panic("unported: mj-transcript/src/projection/terminals.rs finalize_fallback_terminal_tool")
}

// uniquelyMatchingRawTerminal is the one parked terminal result an ACP tool
// demonstrably owns through its raw result. Ambiguous identical results stay
// standalone rather than being assigned to an arbitrary concurrent tool.
func uniquelyMatchingRawTerminal(current *core.MaterializedSession, call *core.OpaqueToolCall) (string, bool) {
	panic("unported: mj-transcript/src/projection/terminals.rs uniquely_matching_raw_terminal")
}

// attachTerminalOutputs remembers every terminal the current call refers
// to, moves any parked output for the terminals item has ever referred to
// into the item, and removes the standalone items it consumed. An exact
// provider raw result also claims its one matching parked terminal: Kimi
// supplies that result but no ACP terminal reference. Output that arrives
// before the tool call therefore ends up exactly where output that arrives
// after it does, and a call that drops its terminal reference on a later
// content update still owns the terminal it started.
func attachTerminalOutputs(current *core.MaterializedSession, index *ProjectionIndex, mutation *core.MaterializedSessionMutation, item *core.TranscriptItem) {
	panic("unported: mj-transcript/src/projection/terminals.rs attach_terminal_outputs")
}

func canonicalTerminalOutput(record *core.TerminalOutputRecord) core.CanonicalTerminalOutput {
	return core.CanonicalTerminalOutput{
		TerminalID: record.TerminalID,
		Output:     record.Output.Text(),
		Truncated:  record.Truncated,
		ExitCode:   record.ExitCode,
		Signal:     record.Signal,
	}
}

func materializedTerminalOutput(record *core.CanonicalTerminalOutput) core.TerminalOutputRecord {
	return core.TerminalOutputRecord{
		TerminalID: record.TerminalID,
		Output:     core.NewPackedText(record.Output),
		Truncated:  record.Truncated,
		ExitCode:   record.ExitCode,
		Signal:     record.Signal,
	}
}
