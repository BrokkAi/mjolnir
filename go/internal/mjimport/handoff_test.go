// Port of the tests in mj-import/src/handoff.rs.

package mjimport

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// handoffFakeBackend is a summarizer that returns a recognizable snapshot
// for any prompt.
type handoffFakeBackend struct{ CompactionBackendDefaults }

func (handoffFakeBackend) Compact(context.Context, string) (string, error) {
	return "<state_snapshot>kept</state_snapshot>", nil
}

// handoffFailingBackend is a summarizer that resolves but fails every
// request with a fatal error.
type handoffFailingBackend struct{ CompactionBackendDefaults }

func (handoffFailingBackend) Compact(context.Context, string) (string, error) {
	return "", errors.New("summarizer exploded")
}

func oneExchangeSnapshot() *core.CanonicalSessionSnapshot {
	bodies := []core.CanonicalTranscriptBody{
		core.CanonicalTranscriptBodyUser{
			Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"fix the bug"}`)},
		},
		core.CanonicalTranscriptBodyAgent{
			Chunks:    []json.RawMessage{json.RawMessage(`{"content":{"type":"text","text":"done"}}`)},
			Streaming: false,
		},
	}
	transcript := make([]core.CanonicalTranscriptItem, 0, len(bodies))
	for index, body := range bodies {
		transcript = append(transcript, core.CanonicalTranscriptItem{
			StableID: fmt.Sprintf("item-%d", index),
			Position: uint64(index) + 1,
			Body:     body,
		})
	}
	return &core.CanonicalSessionSnapshot{
		EventFrontier:       0,
		EventFrontierDigest: strings.Repeat("0", 64),
		Session: core.CanonicalSessionState{
			Execution: core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle},
		},
		Transcript:    transcript,
		QueuedPrompts: []core.CanonicalQueuedPrompt{},
	}
}

// When a backend resolves, the handoff is the summarizer's snapshot under
// the shared preamble, not a bounded transcript.
//
// Hard-won: 781a40ea: native-continuity recovery handed over only the recent transcript and lost all pre-cutoff context despite an available utility model.
func TestAResolvedBackendProducesASummarizedHandoff(t *testing.T) {
	handoff, err := summarizeOrRecent(context.Background(), "session-under-test", oneExchangeSnapshot(),
		64*1024, handoffFakeBackend{}, 64*1024, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(handoff, HandoffPreamble) {
		t.Fatal(handoff)
	}
	if !strings.Contains(handoff, "<state_snapshot>kept</state_snapshot>") {
		t.Fatal(handoff)
	}
	if strings.Contains(handoff, "No summarizer was available") {
		t.Fatalf("a summarized handoff must not carry the fallback preamble: %s", handoff)
	}
}

// A resolved summarizer that then fails does not cost the transcript: the
// handoff falls back to the bounded recent context instead of erroring.
func TestAFailingSummarizerFallsBackToVerbatim(t *testing.T) {
	handoff, err := summarizeOrRecent(context.Background(), "session-under-test", oneExchangeSnapshot(),
		64*1024, handoffFailingBackend{}, 64*1024, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(handoff, HandoffPreamble) {
		t.Fatal(handoff)
	}
	if !strings.Contains(handoff, "fix the bug") {
		t.Fatal(handoff)
	}
}
