// Port of mj-transcript/src/projection.rs.

// Controller-owned projection of the durable ACP relay stream.
//
// Transcript items are shared by pointer between the actor-owned projection
// and the snapshots it publishes (Rust's Arc<TranscriptItem>). A shared item
// is never changed in place: every edit starts from cloneTranscriptItem and
// publishes a new pointer.

package transcript

import (
	"bytes"
	"cmp"
	"encoding/json"
	"fmt"
	"maps"
	"slices"
	"strings"
	"unicode"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type ProjectedRelayEvent struct {
	Mutation core.MaterializedSessionMutation
}

// ProjectionIndex is ephemeral lookup state for projecting a relay page. It
// is deliberately not part of the serialized session: the durable transcript
// remains canonical, while catch-up avoids rediscovering keyed items and
// open streams with a full transcript walk for every event.
type ProjectionIndex struct {
	transcript          map[string]*core.TranscriptItem
	transcriptPositions map[string]int
	// openAgentStreams and openThoughtStreams are Rust's
	// BTreeSet<(u64, String)>: read them in projectionStream order.
	openAgentStreams   map[projectionStream]struct{}
	openThoughtStreams map[projectionStream]struct{}
	// terminalReferrerIDs is Rust's HashMap<String, BTreeSet<String>>: the
	// stable IDs of the tool items referring to each terminal, read in
	// sorted order.
	terminalReferrerIDs map[string]map[string]struct{}
}

// projectionStream is an open stream's key, ordered by position and then
// stable ID as Rust's (u64, String) tuple is.
type projectionStream struct {
	position uint64
	stableID string
}

func compareProjectionStreams(left, right projectionStream) int {
	if c := cmp.Compare(left.position, right.position); c != 0 {
		return c
	}
	return cmp.Compare(left.stableID, right.stableID)
}

// sortedProjectionStreams is the set in BTreeSet order.
func sortedProjectionStreams(streams map[projectionStream]struct{}) []projectionStream {
	return slices.SortedFunc(maps.Keys(streams), compareProjectionStreams)
}

type HistoricalReferences struct {
	IDs []string
	// Terminals is Rust's BTreeSet<String>: the terminals the events refer
	// to, sorted.
	Terminals []string
}

// HistoricalReferencesOf lists explicit references that may address rows
// older than a live window (Rust's historical_references). Keep identity
// interpretation beside the projector that consumes these events.
func HistoricalReferencesOf(events []core.RelayEvent) (HistoricalReferences, error) {
	ids := map[string]struct{}{}
	terminals := map[string]struct{}{}
	for i := range events {
		event := &events[i]
		switch observation := event.Observation.(type) {
		case core.RelayObservationSessionUpdate:
			update := &observation.Update
			var content []schema.ToolCallContent
			switch {
			case update.ToolCall != nil:
				ids[ToolItemID(string(update.ToolCall.ToolCallID))] = struct{}{}
				content = update.ToolCall.Content
			case update.ToolCallUpdate != nil:
				ids[ToolItemID(string(update.ToolCallUpdate.ToolCallID))] = struct{}{}
				content = update.ToolCallUpdate.Content
			case update.AgentMessageChunk != nil:
				if id := update.AgentMessageChunk.MessageID; id != nil {
					ids["agent:"+string(*id)] = struct{}{}
				}
			case update.AgentThoughtChunk != nil:
				if id := update.AgentThoughtChunk.MessageID; id != nil {
					ids["thought:"+string(*id)] = struct{}{}
				}
			}
			for j := range content {
				if block := &content[j]; block.Kind == schema.ToolCallContentKindTerminal && block.Terminal != nil {
					terminals[string(block.Terminal.TerminalID)] = struct{}{}
				}
			}
		case core.RelayObservationTerminalOutput:
			terminals[observation.TerminalID] = struct{}{}
		case core.RelayObservationCommandStarted:
			historicalCommandReferences(ids, observation.CommandID)
		case core.RelayObservationCommandCompleted:
			historicalCommandReferences(ids, observation.CommandID)
		case core.RelayObservationCommandRejected:
			historicalCommandReferences(ids, observation.CommandID)
		case core.RelayObservationCommandInterrupted:
			historicalCommandReferences(ids, observation.CommandID)
		case core.RelayObservationUserShellOutput:
			historicalCommandReferences(ids, observation.CommandID)
		case core.RelayObservationNotice:
			if event.CommandID != nil {
				ids["system:notice:"+*event.CommandID] = struct{}{}
			}
		case core.RelayObservationMailboxEventsDelivered:
			for _, key := range observation.EventKeys {
				ids[core.MessageItemID(key)] = struct{}{}
			}
			for j := range observation.Events {
				ids[core.MessageItemID(observation.Events[j].Key)] = struct{}{}
			}
			if id, ok := externalEventsItemID(observation.Events); ok {
				ids[id] = struct{}{}
			}
			if steering := observation.Steering; steering != nil {
				ids["user:"+steering.QueuedPromptID] = struct{}{}
				ids["user:"+steering.CommandID] = struct{}{}
			}
		case core.RelayObservationCommandQueued:
			wake, ok := observation.Command.(core.RelayCommandMailboxWake)
			if !ok {
				continue
			}
			for j := range wake.Events {
				ids[core.MessageItemID(wake.Events[j].Key)] = struct{}{}
			}
			if id, ok := externalEventsItemID(wake.Events); ok {
				ids[id] = struct{}{}
			}
		}
	}
	for terminal := range terminals {
		ids[terminalItemID(terminal)] = struct{}{}
		ids[fallbackTerminalToolItemID(terminal)] = struct{}{}
	}
	return HistoricalReferences{
		IDs:       slices.Sorted(maps.Keys(ids)),
		Terminals: slices.Sorted(maps.Keys(terminals)),
	}, nil
}

// historicalCommandReferences adds the user shell and user rows a command's
// lifecycle events can address.
func historicalCommandReferences(ids map[string]struct{}, commandID string) {
	ids[userShellItemID(commandID)] = struct{}{}
	ids["user:"+commandID] = struct{}{}
}

// NewProjectionIndex is ProjectionIndex::new.
func NewProjectionIndex(current *core.MaterializedSession) *ProjectionIndex {
	index := &ProjectionIndex{
		transcript:          make(map[string]*core.TranscriptItem, len(current.Transcript)),
		transcriptPositions: make(map[string]int, len(current.Transcript)),
		openAgentStreams:    map[projectionStream]struct{}{},
		openThoughtStreams:  map[projectionStream]struct{}{},
		terminalReferrerIDs: map[string]map[string]struct{}{},
	}
	for position, item := range current.Transcript {
		index.insertAt(item, position)
	}
	return index
}

func (x *ProjectionIndex) get(stableID string) *core.TranscriptItem {
	return x.transcript[stableID]
}

func (x *ProjectionIndex) position(stableID string) (int, bool) {
	position, ok := x.transcriptPositions[stableID]
	return position, ok
}

func (x *ProjectionIndex) insert(item *core.TranscriptItem) {
	position, ok := x.remove(item.StableID)
	if !ok {
		position = len(x.transcript)
	}
	x.insertAt(item, position)
}

func (x *ProjectionIndex) insertAt(item *core.TranscriptItem, position int) {
	stream := projectionStream{position: item.Position, stableID: item.StableID}
	switch body := item.Body.(type) {
	case core.TranscriptBodyAgent:
		if body.Streaming {
			x.openAgentStreams[stream] = struct{}{}
		}
	case core.TranscriptBodyThought:
		if body.Streaming {
			x.openThoughtStreams[stream] = struct{}{}
		}
	case core.TranscriptBodyTool:
		for _, terminalID := range toolTerminalIDs(&body) {
			referrers := x.terminalReferrerIDs[terminalID]
			if referrers == nil {
				referrers = map[string]struct{}{}
				x.terminalReferrerIDs[terminalID] = referrers
			}
			referrers[item.StableID] = struct{}{}
		}
	}
	x.transcriptPositions[item.StableID] = position
	x.transcript[item.StableID] = item
}

func (x *ProjectionIndex) remove(stableID string) (int, bool) {
	item, ok := x.transcript[stableID]
	if !ok {
		return 0, false
	}
	delete(x.transcript, stableID)
	position, ok := x.transcriptPositions[stableID]
	delete(x.transcriptPositions, stableID)
	stream := projectionStream{position: item.Position, stableID: item.StableID}
	delete(x.openAgentStreams, stream)
	delete(x.openThoughtStreams, stream)
	if body, isTool := item.Body.(core.TranscriptBodyTool); isTool {
		for _, terminalID := range toolTerminalIDs(&body) {
			if referrers, found := x.terminalReferrerIDs[terminalID]; found {
				delete(referrers, stableID)
				if len(referrers) == 0 {
					delete(x.terminalReferrerIDs, terminalID)
				}
			}
		}
	}
	return position, ok
}

// toolTerminalIDs is every terminal a tool item refers to: those its current
// call names, then those it remembers.
func toolTerminalIDs(body *core.TranscriptBodyTool) []string {
	terminalIDs := slices.Clone(body.Call.TerminalIDs())
	return append(terminalIDs, body.TerminalRefs...)
}

func (x *ProjectionIndex) reindexAfterRemoval(transcript []*core.TranscriptItem, removed int) {
	for position := removed; position < len(transcript); position++ {
		x.transcriptPositions[transcript[position].StableID] = position
	}
}

// streams is the open agent (agent true) or thought stream set.
func (x *ProjectionIndex) streams(agent bool) map[projectionStream]struct{} {
	if agent {
		return x.openAgentStreams
	}
	return x.openThoughtStreams
}

func (x *ProjectionIndex) latestOpenStream(agent bool) *core.TranscriptItem {
	streams := sortedProjectionStreams(x.streams(agent))
	if len(streams) == 0 {
		return nil
	}
	return x.transcript[streams[len(streams)-1].stableID]
}

func (x *ProjectionIndex) openStreams(agent bool) []*core.TranscriptItem {
	var items []*core.TranscriptItem
	for _, stream := range sortedProjectionStreams(x.streams(agent)) {
		if item, ok := x.transcript[stream.stableID]; ok {
			items = append(items, item)
		}
	}
	return items
}

func (x *ProjectionIndex) TerminalReferrers(terminalID string) []*core.TranscriptItem {
	var items []*core.TranscriptItem
	for _, stableID := range slices.Sorted(maps.Keys(x.terminalReferrerIDs[terminalID])) {
		if item, ok := x.transcript[stableID]; ok {
			items = append(items, item)
		}
	}
	return items
}

// cloneTranscriptItem is Rust's TranscriptItem::clone: a copy whose body
// slices can be edited without touching the shared original (PushContentChunk
// rewrites the last chunk in place).
func cloneTranscriptItem(item *core.TranscriptItem) core.TranscriptItem {
	clone := *item
	switch body := item.Body.(type) {
	case core.TranscriptBodyUser:
		body.Content = slices.Clone(body.Content)
		clone.Body = body
	case core.TranscriptBodyAgent:
		body.Chunks = slices.Clone(body.Chunks)
		clone.Body = body
	case core.TranscriptBodyThought:
		body.Chunks = slices.Clone(body.Chunks)
		clone.Body = body
	case core.TranscriptBodyTool:
		body.TerminalOutputs = slices.Clone(body.TerminalOutputs)
		body.TerminalRefs = slices.Clone(body.TerminalRefs)
		clone.Body = body
	}
	return clone
}

// ProjectRelayEvent derives the minimal mutation for exactly the next relay
// event. This clones only logical items touched by the event; the
// actor-owned transcript is not copied.
func ProjectRelayEvent(current *core.MaterializedSession, event *core.RelayEvent) (ProjectedRelayEvent, error) {
	return ProjectRelayEventIndexed(current, NewProjectionIndex(current), event)
}

func ProjectRelayEventIndexed(current *core.MaterializedSession, index *ProjectionIndex, event *core.RelayEvent) (ProjectedRelayEvent, error) {
	if err := core.ValidateRelayEvent(current.AppliedEventOrdinal, current.AppliedEventDigest, event); err != nil {
		return ProjectedRelayEvent{}, err
	}
	recordedAtMs := event.RecordedAtMs
	mutation := core.MaterializedSessionMutation{LastActivityAtMs: &recordedAtMs}
	if err := projectObservation(current, index, event, &mutation); err != nil {
		return ProjectedRelayEvent{}, err
	}
	mutation.APIEvents = apiEventsDerive(current, event, &mutation)
	return ProjectedRelayEvent{Mutation: mutation}, nil
}

// ApplyCommittedProjectionEvent applies a mutation to the actor's sole
// in-memory projection after the same mutation and frontier have committed
// atomically in SQLite. The mutation's committed values move into the
// projection instead of being copied a second time.
func ApplyCommittedProjectionEvent(current *core.MaterializedSession, event *core.RelayEvent, mutation core.MaterializedSessionMutation) error {
	return applyCommittedProjectionEventInner(current, event, mutation, nil)
}

func ApplyCommittedProjectionEventIndexed(current *core.MaterializedSession, index *ProjectionIndex, event *core.RelayEvent, mutation core.MaterializedSessionMutation) error {
	return applyCommittedProjectionEventInner(current, event, mutation, index)
}

// applyCommittedProjectionEventInner applies mutation; index is nil when
// the caller keeps no ProjectionIndex.
func applyCommittedProjectionEventInner(current *core.MaterializedSession, event *core.RelayEvent, mutation core.MaterializedSessionMutation, index *ProjectionIndex) error {
	if err := core.ValidateRelayEvent(current.AppliedEventOrdinal, current.AppliedEventDigest, event); err != nil {
		return err
	}
	if mutation.Execution != nil {
		current.Execution = *mutation.Execution
	}
	if mutation.SessionTitle != nil {
		current.SessionTitle = *mutation.SessionTitle
	}
	if mutation.Configuration != nil {
		current.Configuration = *mutation.Configuration
	}
	for _, itemMutation := range mutation.Transcript {
		switch change := itemMutation.(type) {
		case core.TranscriptMutationUpsert:
			item := change.Item
			if err := item.Validate(event.Ordinal); err != nil {
				return err
			}
			existingPosition := -1
			if index != nil {
				if position, ok := index.position(item.StableID); ok {
					if position >= len(current.Transcript) {
						return fmt.Errorf("transcript index position %d for %s is out of bounds",
							position, projectionDebug(item.StableID))
					}
					existingPosition = position
				}
			} else {
				existingPosition = slices.IndexFunc(current.Transcript, func(existing *core.TranscriptItem) bool {
					return existing.StableID == item.StableID
				})
			}
			if existingPosition >= 0 {
				existing := current.Transcript[existingPosition]
				if existing.StableID != item.StableID {
					return core.ProjectionIntegrityError(fmt.Sprintf("transcript index for %s points to %s",
						projectionDebug(item.StableID), projectionDebug(existing.StableID)))
				}
				if existing.Position != item.Position || existing.CreatedAtMs != item.CreatedAtMs {
					return core.ProjectionIntegrityError(fmt.Sprintf(
						"transcript item %s changed immutable identity fields", projectionDebug(item.StableID)))
				}
				if item.LastChangedAtMs < existing.LastChangedAtMs {
					return core.ProjectionIntegrityError(fmt.Sprintf(
						"transcript item %s moved its changed timestamp backwards", projectionDebug(item.StableID)))
				}
				if existing.LatestContentEventOrdinal != nil &&
					(item.LatestContentEventOrdinal == nil ||
						*item.LatestContentEventOrdinal < *existing.LatestContentEventOrdinal) {
					return core.ProjectionIntegrityError(fmt.Sprintf(
						"transcript item %s moved its latest content ordinal backwards", projectionDebug(item.StableID)))
				}
				// Snapshots taken earlier may share the existing item, so the
				// changed item is published as a fresh pointer and those
				// snapshots keep the value they were given.
				next := &item
				current.Transcript[existingPosition] = next
				if index != nil {
					index.insert(next)
				}
			} else {
				next := &item
				if index != nil {
					index.insert(next)
				}
				current.Transcript = append(current.Transcript, next)
			}
		case core.TranscriptMutationRemove:
			if index != nil {
				if position, ok := index.remove(change.StableID); ok {
					removed := current.Transcript[position]
					current.Transcript = slices.Delete(current.Transcript, position, position+1)
					if removed.StableID != change.StableID {
						return core.ProjectionIntegrityError(fmt.Sprintf("transcript index for %s removed %s",
							projectionDebug(change.StableID), projectionDebug(removed.StableID)))
					}
					index.reindexAfterRemoval(current.Transcript, position)
				}
			} else {
				current.Transcript = slices.DeleteFunc(current.Transcript, func(item *core.TranscriptItem) bool {
					return item.StableID == change.StableID
				})
			}
		}
	}
	if mutation.QueuedPrompts != nil {
		current.QueuedPrompts = *mutation.QueuedPrompts
	}
	if mutation.PendingElicitations != nil {
		current.PendingElicitations = *mutation.PendingElicitations
	}
	if mutation.ActiveTurn != nil {
		current.ActiveTurn = *mutation.ActiveTurn
	}
	if mutation.ClearTurnOutcome {
		current.LastTurnOutcome = nil
	}
	if mutation.LastTurnOutcome != nil {
		current.LastTurnOutcome = mutation.LastTurnOutcome
	}
	if mutation.LastActivityAtMs != nil {
		activity := *mutation.LastActivityAtMs
		if current.LastActivityAtMs != nil {
			activity = max(*current.LastActivityAtMs, activity)
		}
		current.LastActivityAtMs = &activity
	}
	current.AppliedEventOrdinal = event.Ordinal
	current.AppliedEventDigest = event.Digest
	return nil
}

// ProjectNativeUpdate projects a child-addressed update using the same
// transcript rules as a root. The owning relay has already validated
// ordering; child events are a sparse subsequence of that relay, so their
// local frontier may skip owner ordinals.
func ProjectNativeUpdate(current *core.MaterializedSession, event *core.RelayEvent, update *schema.SessionUpdate) (core.MaterializedSessionMutation, error) {
	recordedAtMs := event.RecordedAtMs
	mutation := core.MaterializedSessionMutation{LastActivityAtMs: &recordedAtMs}
	if chunk := update.UserMessageChunk; chunk != nil {
		content, err := projectionJSON(chunk.Content)
		if err != nil {
			return core.MaterializedSessionMutation{}, err
		}
		mutation.Transcript = append(mutation.Transcript, core.TranscriptMutationUpsert{Item: core.TranscriptItem{
			StableID:        fmt.Sprintf("native-user:%d", event.Ordinal),
			Position:        event.Ordinal,
			CreatedAtMs:     event.RecordedAtMs,
			LastChangedAtMs: event.RecordedAtMs,
			Body:            core.TranscriptBodyUser{Content: []json.RawMessage{content}},
		}})
		return mutation, nil
	}
	index := NewProjectionIndex(current)
	if err := projectSessionUpdate(current, index, event, update, &mutation); err != nil {
		return core.MaterializedSessionMutation{}, err
	}
	return mutation, nil
}

// projectionJSON is serde_json::to_value for the projection: encoding/json
// without HTML escaping, so stored values are byte-for-byte what serde
// writes.
func projectionJSON(value any) (json.RawMessage, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return json.RawMessage(bytes.TrimSuffix(buffer.Bytes(), []byte("\n"))), nil
}

// projectionDebug is Rust's {:?} of a string, as integrity errors print
// stable IDs.
func projectionDebug(value string) string {
	var builder strings.Builder
	builder.WriteByte('"')
	for _, r := range value {
		switch r {
		case '"':
			builder.WriteString(`\"`)
		case '\\':
			builder.WriteString(`\\`)
		case '\n':
			builder.WriteString(`\n`)
		case '\r':
			builder.WriteString(`\r`)
		case '\t':
			builder.WriteString(`\t`)
		case 0:
			builder.WriteString(`\0`)
		default:
			if unicode.IsPrint(r) || r == ' ' {
				builder.WriteRune(r)
			} else {
				fmt.Fprintf(&builder, `\u{%x}`, r)
			}
		}
	}
	builder.WriteByte('"')
	return builder.String()
}
