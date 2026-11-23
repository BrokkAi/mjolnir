// Port of mj-transcript/src/projection/session_update.rs.

package transcript

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func projectSessionUpdate(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	update *schema.SessionUpdate,
	mutation *core.MaterializedSessionMutation,
) error {
	execution := current.Execution
	if mutation.Execution != nil {
		execution = *mutation.Execution
	}
	_, running := execution.(core.MaterializedExecutionStateRunning)
	switch {
	case update.AgentMessageChunk != nil:
		closeStreamKind(index, mutation, false, event.RecordedAtMs)
		return pushStreamChunk(current, index, mutation, event, true, running, update.AgentMessageChunk)
	case update.AgentThoughtChunk != nil:
		closeStreamKind(index, mutation, true, event.RecordedAtMs)
		return pushStreamChunk(current, index, mutation, event, false, running, update.AgentThoughtChunk)
	// CommandStarted is the controller's canonical local user message.
	case update.UserMessageChunk != nil:
	case update.ToolCall != nil:
		return projectToolCall(current, index, event, update.ToolCall, mutation)
	case update.ToolCallUpdate != nil:
		return projectToolCallUpdate(current, index, event, update.ToolCallUpdate, mutation)
	case update.Plan != nil:
		closeStreams(index, mutation, event.RecordedAtMs)
		plan, err := projectionJSON(update.Plan)
		if err != nil {
			return err
		}
		// A plan belongs to the turn that produced it, so a plan from a
		// turn the harness started on its own must not overwrite the
		// previous turn's plan.
		var latestTurnStartPosition uint64
		for i := len(current.Transcript) - 1; i >= 0; i-- {
			if current.Transcript[i].IsTurnStart() {
				latestTurnStartPosition = current.Transcript[i].Position
				break
			}
		}
		for i := len(current.Transcript) - 1; i >= 0; i-- {
			existing := current.Transcript[i]
			if _, isPlan := existing.Body.(core.TranscriptBodyPlan); !isPlan || existing.Position <= latestTurnStartPosition {
				continue
			}
			item := cloneTranscriptItem(existing)
			item.Body = core.TranscriptBodyPlan{Plan: plan}
			item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
			upsert(mutation, item)
			return nil
		}
		upsert(mutation, core.TranscriptItem{
			StableID:        fmt.Sprintf("plan:%d", event.Ordinal),
			Position:        event.Ordinal,
			CreatedAtMs:     event.RecordedAtMs,
			LastChangedAtMs: event.RecordedAtMs,
			Body:            core.TranscriptBodyPlan{Plan: plan},
		})
	case update.ConfigOptionUpdate != nil:
		configuration := cloneSessionConfiguration(current.Configuration)
		values, err := ConfigurationValues(update.ConfigOptionUpdate.ConfigOptions)
		if err != nil {
			return err
		}
		configuration.Values = values
		mutation.Configuration = &configuration
	case update.CurrentModeUpdate != nil:
		configuration := cloneSessionConfiguration(current.Configuration)
		configuration.Values["mode"] = string(update.CurrentModeUpdate.CurrentModeID)
		mutation.Configuration = &configuration
	case update.SessionInfoUpdate != nil:
		return projectSessionInfoUpdate(current, index, event, update.SessionInfoUpdate, mutation)
	case update.UsageUpdate != nil:
		if cost := update.UsageUpdate.Cost; cost != nil {
			mutation.ProviderCost = &core.ProviderCost{
				Amount:       cost.Amount,
				Currency:     cost.Currency,
				ObservedAtMs: event.RecordedAtMs,
			}
		}
	case update.AvailableCommandsUpdate != nil:
	}
	return nil
}

func projectToolCall(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	call *schema.ToolCall,
	mutation *core.MaterializedSessionMutation,
) error {
	closeStreams(index, mutation, event.RecordedAtMs)
	claimed, err := fallbackTerminalAlreadyClaimed(index, call)
	if err != nil {
		return err
	}
	if claimed {
		return nil
	}
	stableID := ToolItemID(string(call.ToolCallID))
	value, err := projectionJSON(call)
	if err != nil {
		return err
	}
	presentation, err := storedToolCallPresentation(value)
	if err != nil {
		return err
	}
	// Agents re-send a whole `tool_call` for an id they already reported,
	// both when they revise a call and when a resumed session replays its
	// history. Merge into the existing item so the immutable identity
	// fields survive.
	if existing := index.get(stableID); existing != nil {
		item := cloneTranscriptItem(existing)
		body, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			return fmt.Errorf("ACP tool call %s conflicts with transcript item %s", call.ToolCallID, stableID)
		}
		body.Call = core.OpaqueToolCallFromValue(value)
		body.Presentation = presentation
		item.Body = body
		item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
		applyTerminalMetadata(&item, call.Meta)
		attachTerminalOutputs(current, index, mutation, &item)
		if err := consumeFallbackTerminalTools(index, mutation, &item, false); err != nil {
			return err
		}
		if err := finalizeFallbackTerminalTool(&item); err != nil {
			return err
		}
		upsert(mutation, item)
		return nil
	}
	item := core.TranscriptItem{
		StableID:        stableID,
		Position:        event.Ordinal,
		CreatedAtMs:     event.RecordedAtMs,
		LastChangedAtMs: event.RecordedAtMs,
		Body: core.TranscriptBodyTool{
			Call:         core.OpaqueToolCallFromValue(value),
			Presentation: presentation,
		},
	}
	applyTerminalMetadata(&item, call.Meta)
	attachTerminalOutputs(current, index, mutation, &item)
	if err := consumeFallbackTerminalTools(index, mutation, &item, true); err != nil {
		return err
	}
	if err := finalizeFallbackTerminalTool(&item); err != nil {
		return err
	}
	upsert(mutation, item)
	return nil
}

func projectToolCallUpdate(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	update *schema.ToolCallUpdate,
	mutation *core.MaterializedSessionMutation,
) error {
	stableID := ToolItemID(string(update.ToolCallID))
	existing := index.get(stableID)
	if existing == nil {
		// Codex may finish dispatching a historical tool update after
		// `session/load` returns even though Hel intentionally did not
		// replay that tool's creation. The update has no target in the
		// canonical transcript, so it is an observable no-op. Log it and
		// advance the relay frontier instead of pinning every later live
		// event behind provider-local resume noise.
		slog.Warn("ignored ACP update for a tool call absent from the durable transcript",
			"session_id", current.SessionID,
			"tool_call_id", string(update.ToolCallID),
			"has_public_fields", toolCallUpdateHasFields(update))
		return nil
	}
	closeStreams(index, mutation, event.RecordedAtMs)
	item := cloneTranscriptItem(existing)
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return fmt.Errorf("ACP tool call %s conflicts with transcript item %s", update.ToolCallID, stableID)
	}
	materializedCall, err := body.Call.Decode()
	if err != nil {
		return fmt.Errorf("parse materialized ACP tool call %s: %w", update.ToolCallID, err)
	}
	fields, err := unstableToolCallUpdate(update)
	if err != nil {
		return err
	}
	presentationChanged := ToolCallUpdateChangesPresentation(&materializedCall, &fields)
	UpdateToolCall(&materializedCall, &fields)
	status := toolCallStatus(&materializedCall)
	if presentationChanged || body.Presentation == nil || body.Presentation.SummaryVersion < ToolSummaryVersion ||
		(body.Presentation.OutputLineCount != nil &&
			(status == acp.ToolCallStatusPending || status == acp.ToolCallStatusInProgress)) {
		presentation := ToolCallPresentation(&materializedCall)
		body.Presentation = &presentation
	}
	value, err := projectionJSON(materializedCall)
	if err != nil {
		return err
	}
	body.Call = core.OpaqueToolCallFromValue(value)
	item.Body = body
	item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
	applyTerminalMetadata(&item, update.Meta)
	attachTerminalOutputs(current, index, mutation, &item)
	if err := consumeFallbackTerminalTools(index, mutation, &item, false); err != nil {
		return err
	}
	if err := finalizeFallbackTerminalTool(&item); err != nil {
		return err
	}
	upsert(mutation, item)
	return nil
}

func projectSessionInfoUpdate(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	update *schema.SessionInfoUpdate,
	mutation *core.MaterializedSessionMutation,
) error {
	configuration := cloneSessionConfiguration(current.Configuration)
	var goal core.GoalState
	if configuration.Goal != nil {
		goal = *configuration.Goal
	}
	configuration.Goal = nil
	changed, err := goal.Apply(&schema.SessionUpdate{Kind: schema.SessionUpdateKindSessionInfoUpdate, SessionInfoUpdate: update})
	if err != nil {
		return err
	}
	if changed {
		configuration.Goal = &goal
		mutation.Configuration = &configuration
	}
	switch {
	case !update.Title.Set:
	case update.Title.Null:
		var none *string
		mutation.SessionTitle = &none
	default:
		var title *string
		if normalized, ok := core.NormalizeSessionTitle(update.Title.Value); ok {
			title = &normalized
		}
		mutation.SessionTitle = &title
	}
	// A bridge that sends typed failures sends its warnings, retries and
	// errors outside a turn this way rather than as agent text. One line
	// per record: a later revision rewrites it in place.
	if failure := core.SessionFailureFromMeta(update.Meta); failure != nil {
		label := "warning"
		if failure.IsError() {
			label = "error"
		}
		text := fmt.Sprintf("%s: %s", label, strings.TrimSpace(failure.Title))
		if failure.Details != nil {
			text = fmt.Sprintf("%s: %s", text, strings.TrimSpace(*failure.Details))
		}
		stableID := "session-failure:" + failure.ID
		var item core.TranscriptItem
		if existing := index.get(stableID); existing != nil {
			item = cloneTranscriptItem(existing)
			item.Body = core.TranscriptBodySystem{Text: text}
			item.LastChangedAtMs = event.RecordedAtMs
		} else {
			item = core.TranscriptItem{
				StableID:        stableID,
				Position:        event.Ordinal,
				CreatedAtMs:     event.RecordedAtMs,
				LastChangedAtMs: event.RecordedAtMs,
				Body:            core.TranscriptBodySystem{Text: text},
			}
		}
		upsert(mutation, item)
	}
	return nil
}

// toolCallUpdateHasFields reports whether an update carries any public
// field (Rust's fields != ToolCallUpdateFields::default()).
func toolCallUpdateHasFields(update *schema.ToolCallUpdate) bool {
	return update.Kind != nil || update.Status != nil || update.Title != nil || update.Name != nil ||
		update.Content != nil || update.Locations != nil || update.RawInput != nil || update.RawOutput != nil
}

// storedToolCallPresentation is the presentation stored with a call, given
// its ACP JSON.
func storedToolCallPresentation(value json.RawMessage) (*core.ToolCallPresentation, error) {
	call, err := unstableToolCall(value)
	if err != nil {
		return nil, err
	}
	presentation := ToolCallPresentation(&call)
	return &presentation, nil
}

// unstableToolCall reads an ACP tool call as the unstable schema type the
// presentation helpers take; both schemas share its wire form.
func unstableToolCall(value json.RawMessage) (acp.ToolCall, error) {
	var call acp.ToolCall
	if err := json.Unmarshal(value, &call); err != nil {
		return acp.ToolCall{}, fmt.Errorf("read ACP tool call: %w", err)
	}
	return call, nil
}

// unstableToolCallUpdate is an update's fields as the unstable schema type.
// Rust applies ToolCallUpdateFields, which carries no _meta, so the update's
// metadata is left out: it never replaces the call's own.
func unstableToolCallUpdate(update *schema.ToolCallUpdate) (acp.ToolCallUpdate, error) {
	fields := *update
	fields.Meta = nil
	encoded, err := projectionJSON(fields)
	if err != nil {
		return acp.ToolCallUpdate{}, err
	}
	var converted acp.ToolCallUpdate
	if err := json.Unmarshal(encoded, &converted); err != nil {
		return acp.ToolCallUpdate{}, fmt.Errorf("read ACP tool call update: %w", err)
	}
	return converted, nil
}

// pushStreamChunk appends an agent (agent true) or thought chunk.
//
// ACP permits trailing session updates after a prompt completes or is
// cancelled (some agents, like Grok Build's goal mode, stream an entire
// autonomous turn this way, one small delta per chunk, and never set
// messageId). A chunk recorded while the session is not running is complete
// by definition, so it must not (re)open a stream: checkpoint export
// requires no open streams at an idle barrier. Instead, a no-message-id
// chunk recorded while idle is coalesced into the transcript's last item
// when that item is the same kind (Agent for an agent chunk, Thought for a
// thought chunk), staying closed (streaming false). This keeps a long run of
// trailing chunks from Grok Build a single transcript item instead of
// thousands, while still segmenting the transcript naturally: an
// intervening item of another kind (a tool call, a plan update, ...) or a
// thought/agent kind switch makes the transcript's last item mismatch, so
// the next chunk starts a fresh item.
func pushStreamChunk(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	mutation *core.MaterializedSessionMutation,
	event *core.RelayEvent,
	agent bool,
	running bool,
	chunk *schema.ContentChunk,
) error {
	prefix := "thought:"
	if agent {
		prefix = "agent:"
	}
	var existing *core.TranscriptItem
	if chunk.MessageID != nil {
		existing = index.get(prefix + string(*chunk.MessageID))
	} else {
		existing = index.latestOpenStream(agent)
	}
	value, err := projectionJSON(chunk)
	if err != nil {
		return err
	}
	ordinal := event.Ordinal
	if existing != nil {
		item := cloneTranscriptItem(existing)
		switch body := item.Body.(type) {
		case core.TranscriptBodyAgent:
			if !agent {
				return fmt.Errorf("ACP message ID conflicts with transcript item %s", item.StableID)
			}
			core.PushContentChunk(&body.Chunks, value)
			body.Streaming = running
			item.Body = body
		case core.TranscriptBodyThought:
			if agent {
				return fmt.Errorf("ACP message ID conflicts with transcript item %s", item.StableID)
			}
			core.PushContentChunk(&body.Chunks, value)
			body.Streaming = running
			item.Body = body
		default:
			return fmt.Errorf("ACP message ID conflicts with transcript item %s", item.StableID)
		}
		item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
		if agent {
			item.LatestContentEventOrdinal = &ordinal
		}
		upsert(mutation, item)
		return nil
	}
	// No message ID and no open same-kind stream: while idle, coalesce into
	// the transcript's last item rather than opening a new item per chunk,
	// as long as that last item is the same kind. The item stays closed; see
	// the running doc comment above.
	if chunk.MessageID == nil && !running && len(current.Transcript) > 0 {
		last := current.Transcript[len(current.Transcript)-1]
		_, lastAgent := last.Body.(core.TranscriptBodyAgent)
		_, lastThought := last.Body.(core.TranscriptBodyThought)
		if (lastAgent && agent) || (lastThought && !agent) {
			item := cloneTranscriptItem(last)
			switch body := item.Body.(type) {
			case core.TranscriptBodyAgent:
				core.PushContentChunk(&body.Chunks, value)
				item.Body = body
			case core.TranscriptBodyThought:
				core.PushContentChunk(&body.Chunks, value)
				item.Body = body
			}
			item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
			if agent {
				item.LatestContentEventOrdinal = &ordinal
			}
			upsert(mutation, item)
			return nil
		}
	}
	stableID := fmt.Sprintf("%s%d", prefix, event.Ordinal)
	if chunk.MessageID != nil {
		stableID = prefix + string(*chunk.MessageID)
	}
	item := core.TranscriptItem{
		StableID:        stableID,
		Position:        event.Ordinal,
		CreatedAtMs:     event.RecordedAtMs,
		LastChangedAtMs: event.RecordedAtMs,
	}
	if agent {
		item.LatestContentEventOrdinal = &ordinal
		item.Body = core.TranscriptBodyAgent{Chunks: []json.RawMessage{value}, Streaming: running}
	} else {
		item.Body = core.TranscriptBodyThought{Chunks: []json.RawMessage{value}, Streaming: running}
	}
	upsert(mutation, item)
	return nil
}

func closeStreamKind(index *ProjectionIndex, mutation *core.MaterializedSessionMutation, agent bool, changedAtMs int64) {
	for _, item := range index.openStreams(agent) {
		closed := cloneTranscriptItem(item)
		switch body := closed.Body.(type) {
		case core.TranscriptBodyAgent:
			body.Streaming = false
			closed.Body = body
		case core.TranscriptBodyThought:
			body.Streaming = false
			closed.Body = body
		default:
			panic(fmt.Sprintf("open stream %q is not an agent or thought item", closed.StableID))
		}
		closed.LastChangedAtMs = max(closed.LastChangedAtMs, changedAtMs)
		upsert(mutation, closed)
	}
}

func closeStreams(index *ProjectionIndex, mutation *core.MaterializedSessionMutation, changedAtMs int64) {
	closeStreamKind(index, mutation, true, changedAtMs)
	closeStreamKind(index, mutation, false, changedAtMs)
}

func pushSystem(mutation *core.MaterializedSessionMutation, event *core.RelayEvent, text string) {
	pushSystemWithID(mutation, event, fmt.Sprintf("system:%d", event.Ordinal), text)
}

func pushSystemWithID(mutation *core.MaterializedSessionMutation, event *core.RelayEvent, stableID string, text string) {
	upsert(mutation, core.TranscriptItem{
		StableID:        stableID,
		Position:        event.Ordinal,
		CreatedAtMs:     event.RecordedAtMs,
		LastChangedAtMs: event.RecordedAtMs,
		Body:            core.TranscriptBodySystem{Text: text},
	})
}

func upsert(mutation *core.MaterializedSessionMutation, item core.TranscriptItem) {
	enrichSettledTool(&item.Body)
	for i, candidate := range mutation.Transcript {
		if current, ok := candidate.(core.TranscriptMutationUpsert); ok && current.Item.StableID == item.StableID {
			mutation.Transcript[i] = core.TranscriptMutationUpsert{Item: item}
			return
		}
	}
	mutation.Transcript = append(mutation.Transcript, core.TranscriptMutationUpsert{Item: item})
}

// ConfigurationValues is each option's current value by option id, in the
// jsonvalue form core.SessionConfiguration.Values holds.
func ConfigurationValues(options []schema.SessionConfigOption) (map[string]any, error) {
	for i := range options {
		if string(options[i].ID) == core.ProjectionKey {
			return nil, errors.New("adapter configuration uses reserved goal state key")
		}
	}
	values := make(map[string]any, len(options))
	for i := range options {
		encoded, err := projectionJSON(options[i])
		if err != nil {
			slog.Warn("could not serialize a session configuration option", "error", err)
			continue
		}
		parsed, err := jsonvalue.Parse(encoded)
		if err != nil {
			slog.Warn("could not serialize a session configuration option", "error", err)
			continue
		}
		object, _ := parsed.(jsonvalue.Object)
		idValue, _ := object.Get("id")
		id, ok := idValue.(string)
		if !ok {
			slog.Warn("session configuration option omitted a string id")
			continue
		}
		current, ok := object.Get("currentValue")
		if !ok {
			current, ok = object.Get("current_value")
		}
		if !ok {
			slog.Warn("session configuration option omitted its current value", "option_id", id)
			continue
		}
		values[id] = current
	}
	return values, nil
}
