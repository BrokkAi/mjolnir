// Port of mj-transcript/src/authorization.rs.
//
// Whole-message authorization evidence shared by workers and controllers.

package transcript

import (
	"encoding/json"
	"errors"
	"iter"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Evidence is the authorization evidence of a materialized session.
func Evidence(session *core.MaterializedSession) (core.ContinuationEvidence, error) {
	return EvidenceFromItems(func(yield func(*core.TranscriptItem, error) bool) {
		for i := len(session.Transcript) - 1; i >= 0; i-- {
			if !yield(session.Transcript[i], nil) {
				return
			}
		}
	})
}

// EvidenceFromItems reads transcript items newest first; an error from the
// sequence ends the read with that error.
func EvidenceFromItems(items iter.Seq2[*core.TranscriptItem, error]) (core.ContinuationEvidence, error) {
	messages := []core.EvidenceMessage{}
	userBytes := 0
	assistantBytes := 0
	assistantHistoryOmitted := false
	// Where the window ends when the budget stops it: replies older than the
	// oldest kept user message answered an evicted one and go with it.
	window := 0
	// Whole recent assistant messages and the newest whole real user
	// messages that fit the budget, the same window the worker keeps live.
	// Reverse then restore chronological order; never turn a clipped
	// instruction into consent.
	var failure error
	for item, err := range items {
		if err != nil {
			failure = err
			break
		}
		if core.IsContextBoundary(item.StableID) {
			break
		}
		var role, text string
		stop := false
		switch body := item.Body.(type) {
		case core.TranscriptBodyUser:
			text = core.MaterializedContentText(body.Content)
			if !core.IsUserAuthoredMessage(&item.StableID, text) {
				continue
			}
			for _, value := range body.Content {
				if kind, ok := transcriptGetString(transcriptParseValue(value), "type"); !ok || kind != "text" {
					failure = errors.New("authorization depends on non-text context")
					break
				}
			}
			if failure != nil {
				break
			}
			if command, _, ok := core.ContextCommandText(text); ok && command == core.ContextCommandClear {
				stop = true
				break
			}
			text = core.TrimMiddle(text)
			if userBytes+len(text) > core.UserBytes {
				assistantHistoryOmitted = assistantHistoryOmitted || len(messages) > window
				messages = messages[:min(window, len(messages))]
				stop = true
				break
			}
			userBytes += len(text)
			role = "user"
		case core.TranscriptBodyAgent:
			if body.Streaming {
				failure = errors.New("assistant reply is still streaming")
				break
			}
			text = core.MaterializedChunksText(body.Chunks)
			if strings.TrimSpace(text) == "" {
				continue
			}
			if assistantHistoryOmitted || assistantBytes+len(text) > core.AssistantBytes {
				if assistantBytes == 0 {
					failure = errors.New("final assistant reply exceeds evidence budget")
					break
				}
				assistantHistoryOmitted = true
				continue
			}
			assistantBytes += len(text)
			role = "assistant"
		default:
			continue
		}
		if failure != nil || stop {
			break
		}
		if strings.TrimSpace(text) != "" {
			if len(messages) >= core.MaxMessages {
				assistantHistoryOmitted = assistantHistoryOmitted || len(messages) > window
				messages = messages[:min(window, len(messages))]
				break
			}
			messages = append(messages, core.EvidenceMessage{ID: item.StableID, Role: role, Text: text})
			if role == "user" {
				window = len(messages)
			}
		}
	}
	if failure != nil {
		return core.ContinuationEvidence{}, failure
	}
	slices.Reverse(messages)
	evidence := core.ContinuationEvidence{Messages: messages, AssistantHistoryOmitted: assistantHistoryOmitted}
	if err := evidence.Validate(); err != nil {
		return core.ContinuationEvidence{}, err
	}
	return evidence, nil
}

// transcriptParseValue reads a stored JSON value in jsonvalue form; nil
// (JSON null) when it is not valid JSON.
func transcriptParseValue(raw json.RawMessage) any {
	value, err := jsonvalue.Parse(raw)
	if err != nil {
		return nil
	}
	return value
}
