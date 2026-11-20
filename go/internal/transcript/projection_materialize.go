// Port of mj-transcript/src/projection/materialize.rs.

package transcript

import (
	"encoding/json"
	"slices"

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
	panic("unported: mj-transcript/src/projection/materialize.rs materialized_session_from_entries")
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
	panic("unported: mj-transcript/src/projection/materialize.rs apply_imported_event")
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
	panic("unported: mj-transcript/src/projection/materialize.rs canonical_session_from_materialized_owned")
}

// canonicalItemOwned converts an item nobody else holds. Rust moves its
// strings and chunks; Go shares them, so it is canonicalItemShared's
// conversion of an unshared item.
func canonicalItemOwned(item core.TranscriptItem) core.CanonicalTranscriptItem {
	panic("unported: mj-transcript/src/projection/materialize.rs canonical_item_owned")
}

func canonicalItemShared(item *core.TranscriptItem) core.CanonicalTranscriptItem {
	panic("unported: mj-transcript/src/projection/materialize.rs canonical_item_shared")
}

func MaterializedSessionFromCanonical(sessionID string, canonical *core.CanonicalSessionSnapshot) (core.MaterializedSession, error) {
	panic("unported: mj-transcript/src/projection/materialize.rs materialized_session_from_canonical")
}

// MaterializedQueuedPromptsFromCanonical projects archived queued commands
// onto the durable queue shape. Kept apart from the whole-projection build
// so a caller that only has to restore the queue does not have to rebuild
// the transcript with it.
func MaterializedQueuedPromptsFromCanonical(queuedPrompts []core.CanonicalQueuedPrompt) []core.MaterializedQueuedPrompt {
	panic("unported: mj-transcript/src/projection/materialize.rs materialized_queued_prompts_from_canonical")
}

// coalescedChunks clones stored ACP content chunks with runs of streamed
// text merged.
func coalescedChunks(chunks []json.RawMessage) []json.RawMessage {
	chunks = slices.Clone(chunks)
	core.CoalesceContentChunks(&chunks)
	return chunks
}
