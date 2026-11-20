// Port of mj-transcript/src/projection.rs.

// Controller-owned projection of the durable ACP relay stream.
//
// Transcript items are shared by pointer between the actor-owned projection
// and the snapshots it publishes (Rust's Arc<TranscriptItem>). A shared item
// is never changed in place: every edit starts from cloneTranscriptItem and
// publishes a new pointer.

package transcript

import (
	"cmp"
	"maps"
	"slices"

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
}

// HistoricalReferencesOf lists explicit references that may address rows
// older than a live window (Rust's historical_references). Keep identity
// interpretation beside the projector that consumes these events.
func HistoricalReferencesOf(events []core.RelayEvent) (HistoricalReferences, error) {
	panic("unported: mj-transcript/src/projection.rs historical_references")
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
	panic("unported: mj-transcript/src/projection.rs ProjectionIndex::insert_at")
}

func (x *ProjectionIndex) remove(stableID string) (int, bool) {
	panic("unported: mj-transcript/src/projection.rs ProjectionIndex::remove")
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

func (x *ProjectionIndex) terminalReferrers(terminalID string) []*core.TranscriptItem {
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
	panic("unported: mj-transcript/src/projection.rs apply_committed_projection_event_inner")
}

// ProjectNativeUpdate projects a child-addressed update using the same
// transcript rules as a root. The owning relay has already validated
// ordering; child events are a sparse subsequence of that relay, so their
// local frontier may skip owner ordinals.
func ProjectNativeUpdate(current *core.MaterializedSession, event *core.RelayEvent, update *schema.SessionUpdate) (core.MaterializedSessionMutation, error) {
	panic("unported: mj-transcript/src/projection.rs project_native_update")
}
