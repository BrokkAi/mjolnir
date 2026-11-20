// Port of mj-transcript/src/projection/session_update.rs.

package transcript

import (
	"fmt"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func projectSessionUpdate(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	update *schema.SessionUpdate,
	mutation *core.MaterializedSessionMutation,
) error {
	panic("unported: mj-transcript/src/projection/session_update.rs project_session_update")
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
	panic("unported: mj-transcript/src/projection/session_update.rs push_stream_chunk")
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
	panic("unported: mj-transcript/src/projection/session_update.rs configuration_values")
}
