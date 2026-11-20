// Port of mj-transcript/src/projection/materialize.rs.

package transcript

import (
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"log/slog"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MaterializedSessionFromEntries converts a transcript projection into the
// controller's canonical logical session. The chat view and the native
// importers both build core.ChatEntry values first and land here; live relay
// sessions are projected directly from relay events instead.
func MaterializedSessionFromEntries(
	sessionID string,
	entries []core.ChatEntry,
	latestSeq uint64,
	phase core.WorkerPhase,
	configuration core.SessionConfiguration,
	queuedPrompts []core.MaterializedQueuedPrompt,
	pendingElicitations []core.ElicitationRequest,
) core.MaterializedSession {
	stableIDs := map[string]struct{}{}
	var transcript []*core.TranscriptItem
	for i := range entries {
		entry := &entries[i]
		if entry.StartSeq == 0 {
			continue
		}
		baseID := entryBaseID(entry)
		stableID := baseID
		if _, taken := stableIDs[baseID]; taken {
			stableID = fmt.Sprintf("%s:%d", baseID, entry.StartSeq)
		} else {
			stableIDs[baseID] = struct{}{}
		}
		var timestamp int64
		if entry.RecordedAtMs != nil {
			timestamp = *entry.RecordedAtMs
		}
		var latest *uint64
		if entry.Role == core.ChatRoleAgent {
			seq := entry.Seq
			latest = &seq
		}
		transcript = append(transcript, &core.TranscriptItem{
			StableID:                  stableID,
			Position:                  entry.StartSeq,
			LatestContentEventOrdinal: latest,
			CreatedAtMs:               timestamp,
			LastChangedAtMs:           timestamp,
			Body:                      entryBody(entry, stableID),
		})
	}
	var startedAtMs int64
	for i := len(entries) - 1; i >= 0; i-- {
		if entries[i].Role == core.ChatRoleUser {
			if entries[i].RecordedAtMs != nil {
				startedAtMs = *entries[i].RecordedAtMs
			}
			break
		}
	}
	appliedEventDigest := core.RelayEventGenesisDigest
	if latestSeq != 0 {
		digest := sha256.New()
		digest.Write([]byte("hel-imported-transcript-frontier-v1\x00"))
		digest.Write([]byte(sessionID))
		digest.Write(binary.LittleEndian.AppendUint64(nil, latestSeq))
		appliedEventDigest = core.LowerHex(digest.Sum(nil))
	}
	var lastActivityAtMs *int64
	for i := range entries {
		if recorded := entries[i].RecordedAtMs; recorded != nil && (lastActivityAtMs == nil || *recorded > *lastActivityAtMs) {
			value := *recorded
			lastActivityAtMs = &value
		}
	}
	var execution core.MaterializedExecutionState
	switch phase {
	case core.WorkerPhaseRunning:
		execution = core.MaterializedExecutionStateRunning{StartedAtMs: startedAtMs}
	case core.WorkerPhaseClosing:
		execution = core.MaterializedExecutionStateClosing{}
	case core.WorkerPhaseClosed:
		execution = core.MaterializedExecutionStateClosed{}
	default:
		execution = core.MaterializedExecutionStateIdle{}
	}
	return core.MaterializedSession{
		SessionID:           sessionID,
		AppliedEventOrdinal: latestSeq,
		AppliedEventDigest:  appliedEventDigest,
		LastActivityAtMs:    lastActivityAtMs,
		Execution:           execution,
		Configuration:       configuration,
		Transcript:          transcript,
		QueuedPrompts:       queuedPrompts,
		PendingElicitations: pendingElicitations,
		// Imported transcripts have no relay command journal, so no turn
		// identity can be reconstructed for them.
	}
}

// entryBaseID is the stable identity an entry asks for before duplicates
// are disambiguated by their start sequence.
func entryBaseID(entry *core.ChatEntry) string {
	switch entry.Role {
	case core.ChatRoleUser:
		var candidate *string
		if entry.Source.Item != nil {
			candidate = &entry.Source.Item.StableID
		} else {
			candidate = entry.MessageID
		}
		if candidate != nil && strings.HasPrefix(*candidate, core.ElicitationReplyItemPrefix) {
			return *candidate
		}
		return fmt.Sprintf("user:%d", entry.StartSeq)
	case core.ChatRoleMessage:
		if entry.IncomingMessage == nil {
			panic("message entry metadata")
		}
		return core.MessageItemID(entry.IncomingMessage.Key)
	case core.ChatRoleAgent:
		if entry.MessageID != nil {
			return "agent:" + *entry.MessageID
		}
		return fmt.Sprintf("agent:%d", entry.StartSeq)
	case core.ChatRoleThought:
		if entry.MessageID != nil {
			return "thought:" + *entry.MessageID
		}
		return fmt.Sprintf("thought:%d", entry.StartSeq)
	case core.ChatRoleTool:
		if entry.ToolCallID != nil {
			return "tool:" + *entry.ToolCallID
		}
		return fmt.Sprintf("tool:%d", entry.StartSeq)
	case core.ChatRolePlan:
		return fmt.Sprintf("plan:%d", entry.StartSeq)
	case core.ChatRolePlanProposal:
		return fmt.Sprintf("plan-proposal:%d", entry.StartSeq)
	}
	return fmt.Sprintf("system:%d", entry.StartSeq)
}

// entryBody is the transcript body one chat entry becomes.
func entryBody(entry *core.ChatEntry, stableID string) core.TranscriptBody {
	switch entry.Role {
	case core.ChatRoleUser:
		content := mustProjectionJSON(jsonvalue.Object{
			{Key: "type", Value: "text"},
			{Key: "text", Value: entry.Text},
		}, "user text content")
		return core.TranscriptBodyUser{Content: []json.RawMessage{content}}
	case core.ChatRoleMessage:
		if entry.IncomingMessage == nil {
			panic("message entry metadata")
		}
		event := *entry.IncomingMessage
		return core.TranscriptBodyMessage{Event: &event}
	case core.ChatRoleAgent, core.ChatRoleThought:
		chunk := schema.ContentChunk{Content: textBlock(entry.Text)}
		if entry.MessageID != nil {
			id := schema.MessageId(*entry.MessageID)
			chunk.MessageID = &id
		}
		chunks := []json.RawMessage{mustProjectionJSON(chunk, "ACP content chunk")}
		if entry.Role == core.ChatRoleAgent {
			return core.TranscriptBodyAgent{Chunks: chunks}
		}
		return core.TranscriptBodyThought{Chunks: chunks}
	case core.ChatRoleTool:
		return entryToolBody(entry, stableID)
	case core.ChatRolePlan:
		plan := schema.Plan{Entries: make([]schema.PlanEntry, 0, len(entry.Plan))}
		for _, line := range entry.Plan {
			status := schema.PlanEntryStatusPending
			switch line.Status {
			case core.PlanStatusRunning:
				status = schema.PlanEntryStatusInProgress
			case core.PlanStatusCompleted:
				status = schema.PlanEntryStatusCompleted
			}
			plan.Entries = append(plan.Entries, schema.PlanEntry{
				Content:  line.Text,
				Priority: schema.PlanEntryPriorityMedium,
				Status:   status,
			})
		}
		return core.TranscriptBodyPlan{Plan: mustProjectionJSON(plan, "ACP plan")}
	case core.ChatRolePlanProposal:
		return core.TranscriptBodyPlanProposal{
			ProposalID: fmt.Sprintf("legacy:%d", entry.StartSeq),
			Plan:       entry.Text,
		}
	}
	return core.TranscriptBodySystem{Text: entry.Text}
}

func entryToolBody(entry *core.ChatEntry, stableID string) core.TranscriptBody {
	callID := stableID
	if entry.ToolCallID != nil {
		callID = *entry.ToolCallID
	}
	toolStatus := core.ToolStatusPending
	if entry.ToolStatus != nil {
		toolStatus = *entry.ToolStatus
	}
	status := schema.ToolCallStatusPending
	switch toolStatus {
	case core.ToolStatusRunning:
		status = schema.ToolCallStatusInProgress
	case core.ToolStatusCompleted:
		status = schema.ToolCallStatusCompleted
	case core.ToolStatusFailed:
		status = schema.ToolCallStatusFailed
	}
	call := schema.ToolCall{
		ToolCallID: schema.ToolCallId(callID),
		Title:      entry.Text,
		Status:     &status,
	}
	for _, text := range entry.ToolContent {
		block := textBlock(text)
		call.Content = append(call.Content, schema.ToolCallContent{
			Kind:    schema.ToolCallContentKindContent,
			Content: &schema.Content{Content: block},
		})
	}
	if entry.ToolInput != nil {
		call.RawInput = slices.Clone(entry.ToolInput)
	}
	var rawOutput jsonvalue.Object
	if entry.ToolError != nil {
		rawOutput = append(rawOutput, jsonvalue.Member{Key: "error", Value: *entry.ToolError})
	}
	if len(entry.ToolDiffstats) > 0 || len(entry.ToolLocations) > 0 {
		rawOutput = append(rawOutput,
			jsonvalue.Member{Key: "legacyDiffstats", Value: stringValues(entry.ToolDiffstats)},
			jsonvalue.Member{Key: "legacyLocations", Value: stringValues(entry.ToolLocations)})
	}
	if len(rawOutput) > 0 {
		call.RawOutput = mustProjectionJSON(rawOutput, "legacy tool raw output")
	}
	value := mustProjectionJSON(call, "ACP tool call")
	presentation := entry.ToolPresentation
	if presentation != nil {
		copied := *presentation
		presentation = &copied
	} else {
		unstable, err := unstableToolCall(value)
		if err != nil {
			panic(fmt.Sprintf("ACP tool call conversion cannot fail: %v", err))
		}
		derived := ToolCallPresentation(&unstable)
		presentation = &derived
	}
	return core.TranscriptBodyTool{
		Call:         core.OpaqueToolCallFromValue(value),
		Presentation: presentation,
	}
}

// stringValues is a string list as JSON array members.
func stringValues(values []string) []any {
	out := make([]any, len(values))
	for i, value := range values {
		out[i] = value
	}
	return out
}

// textBlock is ContentBlock::Text(TextContent::new(text)).
func textBlock(text string) schema.ContentBlock {
	return schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}
}

// mustProjectionJSON serializes values whose serialization cannot fail.
func mustProjectionJSON(value any, what string) json.RawMessage {
	encoded, err := projectionJSON(value)
	if err != nil {
		panic(fmt.Sprintf("%s serialization cannot fail: %v", what, err))
	}
	return encoded
}

// ImportedMaterializedSession projects the relay events a native importer
// synthesized into the canonical logical session it archives. Importers
// replay a harness transcript as prompts, turn boundaries, and agent or
// thought text; the entry building itself is the transcript module's, so an
// imported transcript reads exactly as the same events would in the chat
// view.
func ImportedMaterializedSession(sessionID string, events []core.SequencedEvent) core.MaterializedSession {
	var entries []core.ChatEntry
	phase := core.WorkerPhaseIdle
	var latestSeq uint64
	for i := range events {
		event := &events[i]
		if event.Seq <= latestSeq {
			continue
		}
		applyImportedEvent(&entries, &phase, event)
		latestSeq = event.Seq
	}
	return MaterializedSessionFromEntries(sessionID, entries, latestSeq, phase, core.SessionConfiguration{}, nil, nil)
}

// applyImportedEvent is the transcript and lifecycle effect of one imported
// relay event. The chat view applies the same effect alongside its own view
// state.
func applyImportedEvent(entries *[]core.ChatEntry, phase *core.WorkerPhase, event *core.SequencedEvent) {
	switch worker := event.Event.(type) {
	case core.WorkerEventPromptAccepted:
		*phase = core.WorkerPhaseRunning
		*entries = append(*entries, core.ChatEntryPlain(event.Seq, core.ChatRoleUser, worker.Text).WithRecordedAt(event.RecordedAtMs))
	case core.WorkerEventQueuedPromptPromoted:
		*phase = core.WorkerPhaseRunning
		*entries = append(*entries, core.ChatEntryPlain(event.Seq, core.ChatRoleUser, worker.Prompt.Text).WithRecordedAt(event.RecordedAtMs))
	case core.WorkerEventTurnCompleted:
		*phase = core.WorkerPhaseIdle
	case core.WorkerEventCancelled:
		*phase = core.WorkerPhaseRunning
	case core.WorkerEventClosing:
		*phase = core.WorkerPhaseClosing
	case core.WorkerEventClosed:
		*phase = core.WorkerPhaseClosed
	case core.WorkerEventAdapter:
		runtime, err := core.UnmarshalRuntimeEvent(worker.Payload)
		if err != nil {
			slog.Warn("ignoring malformed persisted runtime event", "seq", event.Seq, "error", err)
			return
		}
		ApplyRuntimeEventToEntries(entries, event.Seq, event.RecordedAtMs, runtime)
	}
}

// CanonicalSessionFromMaterialized builds the canonical snapshot from a
// borrowed projection. Transcript items are shared by pointer, so this
// copies the handles and converts each body once; use
// CanonicalSessionFromMaterializedOwned when the caller can give the
// projection up.
func CanonicalSessionFromMaterialized(materialized *core.MaterializedSession) (core.CanonicalSessionSnapshot, error) {
	return CanonicalSessionFromMaterializedOwned(*materialized)
}

// CanonicalSessionFromMaterializedOwned builds the canonical snapshot by
// consuming the projection.
func CanonicalSessionFromMaterializedOwned(materialized core.MaterializedSession) (core.CanonicalSessionSnapshot, error) {
	transcript := make([]core.CanonicalTranscriptItem, 0, len(materialized.Transcript))
	for _, item := range materialized.Transcript {
		// Go cannot tell whether a published snapshot shares the item, so
		// every item converts as a shared one; the conversion never copies
		// the item's body deeper than its slices.
		transcript = append(transcript, canonicalItemShared(item))
	}
	var execution core.CanonicalExecutionState
	switch state := materialized.Execution.(type) {
	case core.MaterializedExecutionStateRunning:
		execution = core.CanonicalExecutionState{State: core.CanonicalExecutionStateRunning, StartedAtMs: state.StartedAtMs}
	case core.MaterializedExecutionStateClosing:
		execution = core.CanonicalExecutionState{State: core.CanonicalExecutionStateClosing}
	case core.MaterializedExecutionStateClosed:
		execution = core.CanonicalExecutionState{State: core.CanonicalExecutionStateClosed}
	default:
		execution = core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle}
	}
	queuedPrompts := make([]core.CanonicalQueuedPrompt, 0, len(materialized.QueuedPrompts))
	for _, prompt := range materialized.QueuedPrompts {
		kind := core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt}
		if config, ok := prompt.Kind.(core.QueuedCommandKindSetConfig); ok {
			kind = core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindSetConfig, Key: config.Key, Value: config.Value}
		}
		content, err := queuedContentRaw(prompt.Content)
		if err != nil {
			return core.CanonicalSessionSnapshot{}, err
		}
		queuedPrompts = append(queuedPrompts, core.CanonicalQueuedPrompt{
			CommandID:  prompt.CommandID,
			Kind:       kind,
			Content:    content,
			QueuedAtMs: prompt.QueuedAtMs,
		})
	}
	return core.CanonicalSessionSnapshot{
		EventFrontier:       materialized.AppliedEventOrdinal,
		EventFrontierDigest: materialized.AppliedEventDigest,
		Session: core.CanonicalSessionState{
			Execution:        execution,
			LastActivityAtMs: materialized.LastActivityAtMs,
			SessionTitle:     materialized.SessionTitle,
			Configuration:    materialized.Configuration,
		},
		Transcript:    transcript,
		QueuedPrompts: queuedPrompts,
	}, nil
}

// canonicalItemOwned converts an item nobody else holds. Rust moves its
// strings and chunks; Go shares them, so it is canonicalItemShared's
// conversion of an unshared item.
func canonicalItemOwned(item core.TranscriptItem) core.CanonicalTranscriptItem {
	return canonicalItemShared(&item)
}

func canonicalItemShared(item *core.TranscriptItem) core.CanonicalTranscriptItem {
	var body core.CanonicalTranscriptBody
	switch source := item.Body.(type) {
	case core.TranscriptBodyUser:
		body = core.CanonicalTranscriptBodyUser{Content: slices.Clone(source.Content)}
	case core.TranscriptBodyMessage:
		var event *core.MailboxEvent
		if source.Event != nil {
			copied := *source.Event
			event = &copied
		}
		body = core.CanonicalTranscriptBodyMessage{Event: event}
	case core.TranscriptBodyAgent:
		body = core.CanonicalTranscriptBodyAgent{Chunks: slices.Clone(source.Chunks), Streaming: source.Streaming}
	case core.TranscriptBodyThought:
		body = core.CanonicalTranscriptBodyThought{Chunks: slices.Clone(source.Chunks), Streaming: source.Streaming}
	case core.TranscriptBodyTool:
		var outputs []core.CanonicalTerminalOutput
		for i := range source.TerminalOutputs {
			outputs = append(outputs, canonicalTerminalOutput(&source.TerminalOutputs[i]))
		}
		var presentation *core.ToolCallPresentation
		if source.Presentation != nil {
			copied := *source.Presentation
			presentation = &copied
		}
		body = core.CanonicalTranscriptBodyTool{
			Call:            source.Call.Value(),
			TerminalOutputs: outputs,
			TerminalRefs:    slices.Clone(source.TerminalRefs),
			Presentation:    presentation,
		}
	case core.TranscriptBodyTerminalOutput:
		body = core.CanonicalTranscriptBodyTerminalOutput{Record: canonicalTerminalOutput(&source.Record)}
	case core.TranscriptBodyPlan:
		body = core.CanonicalTranscriptBodyPlan{Plan: slices.Clone(source.Plan)}
	case core.TranscriptBodyPlanProposal:
		body = core.CanonicalTranscriptBodyPlanProposal{ProposalID: source.ProposalID, Plan: source.Plan}
	case core.TranscriptBodySystem:
		body = core.CanonicalTranscriptBodySystem{Text: source.Text}
	}
	return core.CanonicalTranscriptItem{
		StableID:                  item.StableID,
		Position:                  item.Position,
		LatestContentEventOrdinal: item.LatestContentEventOrdinal,
		CreatedAtMs:               item.CreatedAtMs,
		LastChangedAtMs:           item.LastChangedAtMs,
		Body:                      body,
	}
}

func MaterializedSessionFromCanonical(sessionID string, canonical *core.CanonicalSessionSnapshot) (core.MaterializedSession, error) {
	transcript := make([]*core.TranscriptItem, 0, len(canonical.Transcript))
	for i := range canonical.Transcript {
		item := &canonical.Transcript[i]
		var body core.TranscriptBody
		switch source := item.Body.(type) {
		case core.CanonicalTranscriptBodyUser:
			body = core.TranscriptBodyUser{Content: slices.Clone(source.Content)}
		case core.CanonicalTranscriptBodyMessage:
			var event *core.MailboxEvent
			if source.Event != nil {
				copied := *source.Event
				event = &copied
			}
			body = core.TranscriptBodyMessage{Event: event}
		// Chunks stored before streamed text was merged on the way in
		// arrive one per token; collapse them as they come back.
		case core.CanonicalTranscriptBodyAgent:
			body = core.TranscriptBodyAgent{Chunks: coalescedChunks(source.Chunks), Streaming: source.Streaming}
		case core.CanonicalTranscriptBodyThought:
			body = core.TranscriptBodyThought{Chunks: coalescedChunks(source.Chunks), Streaming: source.Streaming}
		case core.CanonicalTranscriptBodyTool:
			var outputs []core.TerminalOutputRecord
			for j := range source.TerminalOutputs {
				outputs = append(outputs, materializedTerminalOutput(&source.TerminalOutputs[j]))
			}
			var presentation *core.ToolCallPresentation
			if source.Presentation != nil {
				copied := *source.Presentation
				presentation = &copied
			}
			body = core.TranscriptBodyTool{
				Call:            core.OpaqueToolCallFromValue(source.Call),
				TerminalOutputs: outputs,
				TerminalRefs:    slices.Clone(source.TerminalRefs),
				Presentation:    presentation,
			}
		case core.CanonicalTranscriptBodyTerminalOutput:
			body = core.TranscriptBodyTerminalOutput{Record: materializedTerminalOutput(&source.Record)}
		case core.CanonicalTranscriptBodyPlan:
			body = core.TranscriptBodyPlan{Plan: slices.Clone(source.Plan)}
		case core.CanonicalTranscriptBodyPlanProposal:
			body = core.TranscriptBodyPlanProposal{ProposalID: source.ProposalID, Plan: source.Plan}
		case core.CanonicalTranscriptBodySystem:
			body = core.TranscriptBodySystem{Text: source.Text}
		}
		transcript = append(transcript, &core.TranscriptItem{
			StableID:                  item.StableID,
			Position:                  item.Position,
			LatestContentEventOrdinal: item.LatestContentEventOrdinal,
			CreatedAtMs:               item.CreatedAtMs,
			LastChangedAtMs:           item.LastChangedAtMs,
			Body:                      body,
		})
	}
	var execution core.MaterializedExecutionState
	switch canonical.Session.Execution.State {
	case core.CanonicalExecutionStateRunning:
		execution = core.MaterializedExecutionStateRunning{StartedAtMs: canonical.Session.Execution.StartedAtMs}
	case core.CanonicalExecutionStateClosing:
		execution = core.MaterializedExecutionStateClosing{}
	case core.CanonicalExecutionStateClosed:
		execution = core.MaterializedExecutionStateClosed{}
	default:
		execution = core.MaterializedExecutionStateIdle{}
	}
	return core.MaterializedSession{
		SessionID:           sessionID,
		AppliedEventOrdinal: canonical.EventFrontier,
		AppliedEventDigest:  canonical.EventFrontierDigest,
		LastActivityAtMs:    canonical.Session.LastActivityAtMs,
		Execution:           execution,
		SessionTitle:        canonical.Session.SessionTitle,
		Configuration:       cloneSessionConfiguration(canonical.Session.Configuration),
		Transcript:          transcript,
		QueuedPrompts:       MaterializedQueuedPromptsFromCanonical(canonical.QueuedPrompts),
		// The checkpoint archive does not carry turn identity, so a resumed
		// session starts with no active turn and no last outcome.
	}, nil
}

// MaterializedQueuedPromptsFromCanonical projects archived queued commands
// onto the durable queue shape. Kept apart from the whole-projection build
// so a caller that only has to restore the queue does not have to rebuild
// the transcript with it.
func MaterializedQueuedPromptsFromCanonical(queuedPrompts []core.CanonicalQueuedPrompt) []core.MaterializedQueuedPrompt {
	var prompts []core.MaterializedQueuedPrompt
	for _, prompt := range queuedPrompts {
		var kind core.QueuedCommandKind = core.QueuedCommandKindPrompt{}
		if prompt.Kind.Kind == core.CanonicalQueuedCommandKindSetConfig {
			kind = core.QueuedCommandKindSetConfig{Key: prompt.Kind.Key, Value: prompt.Kind.Value}
		}
		prompts = append(prompts, core.MaterializedQueuedPrompt{
			CommandID:  prompt.CommandID,
			Kind:       kind,
			Content:    queuedContentValues(prompt.Content),
			QueuedAtMs: prompt.QueuedAtMs,
		})
	}
	return prompts
}

// coalescedChunks clones stored ACP content chunks with runs of streamed
// text merged.
func coalescedChunks(chunks []json.RawMessage) []json.RawMessage {
	chunks = slices.Clone(chunks)
	core.CoalesceContentChunks(&chunks)
	return chunks
}
