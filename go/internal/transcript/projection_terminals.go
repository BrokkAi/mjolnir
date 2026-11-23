// Port of mj-transcript/src/projection/terminals.rs.

package transcript

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"slices"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// terminalOutputLimit is the retained bytes of one terminal's output.
const terminalOutputLimit = 4 * 1024 * 1024

// applyTerminalMetadata folds the ACP v1 display-terminal extension into the
// same durable records used for client-owned terminals. Metadata on updates
// is not part of ToolCallUpdateFields, so applying the update alone loses
// these bytes. A nil meta is Rust's None.
func applyTerminalMetadata(item *core.TranscriptItem, meta jsonvalue.Object) {
	if meta == nil {
		return
	}
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return
	}
	var exit *terminalMetadataExit
	if value, ok := meta.Get("terminal_exit"); ok {
		var decoded terminalMetadataExit
		if err := decodeTerminalMetadata(value, &decoded); err != nil {
			slog.Warn("invalid ACP terminal exit metadata", "error", err)
		} else {
			exit = &decoded
		}
	}
	for _, key := range []string{"terminal_output", "terminal_output_delta", "terminal_exit"} {
		value, ok := meta.Get(key)
		if !ok {
			continue
		}
		var output *terminalMetadataOutput
		if key != "terminal_exit" {
			var decoded terminalMetadataOutput
			if err := decodeTerminalMetadata(value, &decoded); err != nil {
				slog.Warn("invalid ACP terminal output metadata", "error", err, "key", key)
				continue
			}
			output = &decoded
		}
		var id string
		switch {
		case output != nil:
			id = output.TerminalID
		case exit != nil:
			id = exit.TerminalID
		default:
			continue
		}
		if !slices.Contains(body.TerminalRefs, id) {
			body.TerminalRefs = append(body.TerminalRefs, id)
		}
		position := slices.IndexFunc(body.TerminalOutputs, func(record core.TerminalOutputRecord) bool {
			return record.TerminalID == id
		})
		if position < 0 {
			body.TerminalOutputs = append(body.TerminalOutputs, core.TerminalOutputRecord{TerminalID: id})
			position = len(body.TerminalOutputs) - 1
		}
		record := &body.TerminalOutputs[position]
		if output != nil {
			// Older Codex bridges use terminal_output for streamed chunks,
			// then send an exit without output. Output accompanying an exit
			// is a final snapshot; the explicit delta channel always appends.
			if key == "terminal_output" && exit != nil && exit.TerminalID == record.TerminalID {
				record.Output = core.PackedTextPlain(output.Data)
				record.Truncated = false
			} else {
				record.Output = core.PackedTextPlain(record.Output.IntoText() + output.Data)
			}
			text := record.Output.IntoText()
			if core.TruncateStringStart(&text, terminalOutputLimit) {
				record.Truncated = true
			}
			record.Output = core.PackedTextPlain(text)
		} else if exit != nil {
			record.ExitCode = exit.ExitCode
			record.Signal = exit.Signal
			record.Output = core.NewPackedText(record.Output.IntoText())
		}
	}
	item.Body = body
}

// decodeTerminalMetadata reads one metadata member as serde_json::from_value
// reads it into the local metadata structs.
func decodeTerminalMetadata(value any, target any) error {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	return json.Unmarshal(encoded, target)
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
	return ToolItemID(core.FallbackTerminalToolCallID(terminalID))
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

// decodeToolCall is OpaqueToolCall::decode into the stable ACP ToolCall the
// Rust projection reads.
func decodeToolCall(call core.OpaqueToolCall) (schema.ToolCall, error) {
	var decoded schema.ToolCall
	err := json.Unmarshal(call.Value(), &decoded)
	return decoded, err
}

// uniquelyMatchingRawTool is the one provider tool demonstrably owning a
// result through its raw value. Identical concurrent results are
// deliberately left with the fallback tool.
func uniquelyMatchingRawTool(index *ProjectionIndex, record *core.TerminalOutputRecord) *core.TranscriptItem {
	digest := record.RawResultDigest()
	var match *core.TranscriptItem
	for _, item := range index.transcript {
		body, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			continue
		}
		fallback, err := fallbackToolItem(item)
		if err != nil || fallback {
			continue
		}
		if !body.Call.MatchesRawResult(record, &digest) {
			continue
		}
		if match != nil {
			return nil
		}
		match = item
	}
	return match
}

// fallbackTerminalAlreadyClaimed: a provider may publish its real tool
// before it asks the client to create the terminal. In that ordering the
// existing call already provides the durable start item, so the
// compatibility call would only duplicate it.
func fallbackTerminalAlreadyClaimed(index *ProjectionIndex, call *schema.ToolCall) (bool, error) {
	if !core.IsFallbackTerminalToolCall(call) {
		return false, nil
	}
	value, err := projectionJSON(call)
	if err != nil {
		return false, err
	}
	for _, terminalID := range toolCallTerminalIDs(value) {
		for _, item := range index.TerminalReferrers(terminalID) {
			body, ok := item.Body.(core.TranscriptBodyTool)
			if ok && body.Call.TypedValid() && !body.Call.IsFallback() {
				return true, nil
			}
		}
	}
	return false, nil
}

// consumeFallbackTerminalTools replaces Hel's interim terminal tool with the
// real ACP tool once the agent supplies one. Output may already have landed
// on the interim item, so move it along. A newly created real item can also
// adopt the interim item's start identity; an existing tool keeps its
// immutable position and timestamp.
func consumeFallbackTerminalTools(index *ProjectionIndex, mutation *core.MaterializedSessionMutation, item *core.TranscriptItem, mayAdoptIdentity bool) error {
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return nil
	}
	materialized, err := decodeToolCall(body.Call)
	if err != nil {
		return fmt.Errorf("parse ACP tool call while claiming fallback terminal tools: %w", err)
	}
	if core.IsFallbackTerminalToolCall(&materialized) {
		return nil
	}
	for _, terminalID := range toolTerminalIDs(&body) {
		stableID := fallbackTerminalToolItemID(terminalID)
		if stableID == item.StableID {
			continue
		}
		fallback := index.get(stableID)
		if fallback == nil {
			continue
		}
		fallbackBody, ok := fallback.Body.(core.TranscriptBodyTool)
		if !ok {
			continue
		}
		fallbackCall, err := decodeToolCall(fallbackBody.Call)
		if err != nil {
			return fmt.Errorf("parse fallback terminal tool call: %w", err)
		}
		if !core.IsFallbackTerminalToolCall(&fallbackCall) {
			continue
		}
		for _, record := range fallbackBody.TerminalOutputs {
			replaceOrPushTerminalRecord(&body.TerminalOutputs, record)
		}
		for _, terminalRef := range fallbackBody.TerminalRefs {
			if !slices.Contains(body.TerminalRefs, terminalRef) {
				body.TerminalRefs = append(body.TerminalRefs, terminalRef)
			}
		}
		if !slices.Contains(body.TerminalRefs, terminalID) {
			body.TerminalRefs = append(body.TerminalRefs, terminalID)
		}
		item.Body = body
		if mayAdoptIdentity {
			item.Position = min(item.Position, fallback.Position)
			item.CreatedAtMs = min(item.CreatedAtMs, fallback.CreatedAtMs)
		}
		item.LastChangedAtMs = max(item.LastChangedAtMs, fallback.LastChangedAtMs)
		mutation.Transcript = append(mutation.Transcript, core.TranscriptMutationRemove{StableID: stableID})
	}
	item.Body = body
	return nil
}

// finalizeFallbackTerminalTool: terminal close is a Hel event rather than an
// ACP tool update. Complete only the interim call Hel created; a
// provider-owned call keeps its own status.
func finalizeFallbackTerminalTool(item *core.TranscriptItem) error {
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return nil
	}
	if len(body.TerminalOutputs) == 0 {
		return nil
	}
	materialized, err := decodeToolCall(body.Call)
	if err != nil {
		return fmt.Errorf("parse ACP tool call while finalizing fallback terminal tool: %w", err)
	}
	if !core.IsFallbackTerminalToolCall(&materialized) {
		return nil
	}
	status := schema.ToolCallStatusCompleted
	for i := range body.TerminalOutputs {
		if !body.TerminalOutputs[i].ExitedCleanly() {
			status = schema.ToolCallStatusFailed
			break
		}
	}
	materialized.Status = &status
	value, err := projectionJSON(materialized)
	if err != nil {
		return err
	}
	body.Call = core.OpaqueToolCallFromValue(value)
	item.Body = body
	return nil
}

// uniquelyMatchingRawTerminal is the one parked terminal result an ACP tool
// demonstrably owns through its raw result. Ambiguous identical results stay
// standalone rather than being assigned to an arbitrary concurrent tool.
func uniquelyMatchingRawTerminal(current *core.MaterializedSession, call *core.OpaqueToolCall) (string, bool) {
	var terminalID string
	found := false
	for _, item := range current.Transcript {
		var record *core.TerminalOutputRecord
		switch body := item.Body.(type) {
		case core.TranscriptBodyTerminalOutput:
			record = &body.Record
		case core.TranscriptBodyTool:
			fallback, err := fallbackToolItem(item)
			if err != nil || !fallback || len(body.TerminalOutputs) == 0 {
				continue
			}
			record = &body.TerminalOutputs[0]
		default:
			continue
		}
		digest := record.RawResultDigest()
		if !call.MatchesRawResult(record, &digest) {
			continue
		}
		if found {
			return "", false
		}
		terminalID, found = record.TerminalID, true
	}
	return terminalID, found
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
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return
	}
	for _, terminalID := range body.Call.TerminalIDs() {
		if !slices.Contains(body.TerminalRefs, terminalID) {
			body.TerminalRefs = append(body.TerminalRefs, terminalID)
		}
	}
	if len(body.TerminalRefs) == 0 {
		if terminalID, ok := uniquelyMatchingRawTerminal(current, &body.Call); ok && !slices.Contains(body.TerminalRefs, terminalID) {
			body.TerminalRefs = append(body.TerminalRefs, terminalID)
		}
	}
	var consumed []string
	for _, terminalID := range body.TerminalRefs {
		record, ok := findTerminalRecord(index, terminalID)
		if !ok {
			continue
		}
		replaceOrPushTerminalRecord(&body.TerminalOutputs, record)
		consumed = append(consumed, terminalItemID(terminalID))
	}
	for _, stableID := range consumed {
		mutation.Transcript = append(mutation.Transcript, core.TranscriptMutationRemove{StableID: stableID})
	}
	item.Body = body
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

// materializedTerminalOutput is Rust's `record.into()`, mj-core's
// From<&CanonicalTerminalOutput> for TerminalOutputRecord.
func materializedTerminalOutput(record *core.CanonicalTerminalOutput) core.TerminalOutputRecord {
	return core.TerminalOutputRecord{
		TerminalID: record.TerminalID,
		Output:     core.NewPackedText(record.Output),
		Truncated:  record.Truncated,
		ExitCode:   record.ExitCode,
		Signal:     record.Signal,
	}
}
