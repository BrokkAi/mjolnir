// Port of the tests in mj-import/src/handoff/claude.rs.

package mjimport

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

type claudeRecordingBackend struct {
	CompactionBackendDefaults
	mu      sync.Mutex
	prompts []string
}

func (b *claudeRecordingBackend) Compact(_ context.Context, prompt string) (string, error) {
	b.mu.Lock()
	b.prompts = append(b.prompts, prompt)
	b.mu.Unlock()
	return "<state_snapshot>merged</state_snapshot>", nil
}

// cancelAfterLine counts the reads the parser makes and cancels once one
// has returned data (Rust counts fill_buf and cancels on consume).
type cancelAfterLine struct {
	inner  *bytes.Reader
	cancel context.CancelFunc
	reads  *atomic.Int64
}

func (r *cancelAfterLine) Read(buffer []byte) (int, error) {
	r.reads.Add(1)
	count, err := r.inner.Read(buffer)
	if count > 0 {
		r.cancel()
	}
	return count, err
}

func claudeSnapshot(bodies []core.CanonicalTranscriptBody) *core.CanonicalSessionSnapshot {
	activity := int64(900_000)
	items := make([]core.CanonicalTranscriptItem, 0, len(bodies))
	for index, body := range bodies {
		items = append(items, core.CanonicalTranscriptItem{
			StableID:        fmt.Sprintf("entry-%d", index),
			Position:        uint64(index) + 1,
			CreatedAtMs:     900_000,
			LastChangedAtMs: 900_000,
			Body:            body,
		})
	}
	return &core.CanonicalSessionSnapshot{
		EventFrontier:       uint64(len(bodies)) + 10,
		EventFrontierDigest: strings.Repeat("a", 64),
		Session: core.CanonicalSessionState{
			Execution:        core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle},
			LastActivityAtMs: &activity,
		},
		Transcript:    items,
		QueuedPrompts: []core.CanonicalQueuedPrompt{},
	}
}

func claudeUser(text string) core.CanonicalTranscriptBody {
	encoded, _ := json.Marshal(map[string]string{"type": "text", "text": text})
	return core.CanonicalTranscriptBodyUser{Content: []json.RawMessage{encoded}}
}

func claudeTool(id string, terminal bool) core.CanonicalTranscriptBody {
	call, _ := json.Marshal(map[string]string{"toolCallId": id, "title": "Run", "kind": "execute", "status": "completed"})
	body := core.CanonicalTranscriptBodyTool{Call: call}
	if terminal {
		body.TerminalRefs = []string{"terminal-1"}
	}
	return body
}

func claudeAgent(text string) core.CanonicalTranscriptBody {
	chunk, _ := json.Marshal(map[string]any{"content": map[string]string{"type": "text", "text": text}})
	return core.CanonicalTranscriptBodyAgent{Chunks: []json.RawMessage{chunk}}
}

func claudeNative(records []nativeRecord) (*nativeCompaction, []nativeRecord) {
	return &nativeCompaction{
		summary:      "prior state",
		boundaryAtMs: 100,
		summaryAtMs:  101,
		beforeRecord: len(records),
	}, records
}

func nativeUser(text string) nativeRecord { return nativeRecord{kind: nativeRecordUser, text: text} }
func nativeToolUse(id string) nativeRecord {
	return nativeRecord{kind: nativeRecordToolUse, id: id}
}
func nativeToolResult(id string) nativeRecord {
	return nativeRecord{kind: nativeRecordToolResult, id: id}
}

func claudeLines(lines ...string) string { return strings.Join(lines, "\n") }

func TestNativeParserKeepsCompletePairsAndTextBlockSummary(t *testing.T) {
	body := claudeLines(
		`{"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"content":"work"}}`,
		`{"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:01:00Z"}`,
		`{"type":"user","isCompactSummary":true,"timestamp":"2026-01-01T00:01:01Z","message":{"content":"old state"}}`,
		`{"type":"assistant","timestamp":"2026-01-01T00:02:00Z","message":{"content":[{"type":"tool_use","id":"tool-a"}]}}`,
		`{"type":"user","timestamp":"2026-01-01T00:02:01Z","message":{"content":[{"type":"tool_result","tool_use_id":"tool-a"}]}}`,
		`{"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:03:00Z"}`,
		`{"type":"user","isCompactSummary":true,"timestamp":"2026-01-01T00:03:01Z","message":{"content":[{"type":"text","text":"new state"}]}}`,
		`{"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:04:00Z"}`,
		`{"type":"user","timestamp":"2026-01-01T00:04:01Z","message":{"content":"more work"}}`,
	)
	parsed, err := parseNativeCompactions(context.Background(), []byte(body))
	if err != nil {
		t.Fatal(err)
	}
	if len(parsed.compactions) != 2 {
		t.Fatalf("compactions = %+v", parsed.compactions)
	}
	if parsed.compactions[1].summary != "new state" {
		t.Fatalf("summary = %q", parsed.compactions[1].summary)
	}
	if parsed.compactions[1].beforeRecord != 3 {
		t.Fatalf("before record = %d", parsed.compactions[1].beforeRecord)
	}
}

func TestNativeParserObservesCancellationBeforeAndBetweenLargeLines(t *testing.T) {
	first, _ := json.Marshal(map[string]any{"type": "user", "message": map[string]string{"content": strings.Repeat("x", 70*1024)}})
	body := fmt.Sprintf("%s\n%s\n", first, `{"message":{"content":"later"},"type":"user"}`)
	var reads atomic.Int64
	canceled, cancelNow := context.WithCancel(context.Background())
	cancelNow()
	reader := &cancelAfterLine{inner: bytes.NewReader([]byte(body)), cancel: cancelNow, reads: &reads}
	if _, err := parseNativeCompactionsFrom(canceled, reader); err == nil {
		t.Fatal("a cancelled parse succeeded")
	}
	if reads.Load() != 0 {
		t.Fatalf("reads = %d", reads.Load())
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	reader = &cancelAfterLine{inner: bytes.NewReader([]byte(body)), cancel: cancel, reads: &reads}
	_, err := parseNativeCompactionsFrom(ctx, reader)
	if err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("error = %v", err)
	}
	if reads.Load() == 0 {
		t.Fatal("the parser read nothing")
	}
}

// The native and relay clocks need not agree; tool identity establishes the
// cut.
func TestNativeToolBarrierReplaysTheSpanningToolAndAgentSuffix(t *testing.T) {
	canonical := claudeSnapshot([]core.CanonicalTranscriptBody{
		claudeUser("implement it"),
		claudeAgent("working"),
		claudeTool("tool-a", false),
		claudeAgent("continued"),
	})
	ordinal := uint64(2)
	canonical.Transcript[1].LatestContentEventOrdinal = &ordinal
	pair, records := claudeNative([]nativeRecord{
		nativeUser("implement it"),
		nativeToolUse("tool-a"),
		nativeToolResult("tool-a"),
	})
	prior := mapCompaction(pair, records, nil, canonical)
	if prior == nil {
		t.Fatal("no prior compaction")
	}
	if prior.ThroughPosition != 2 {
		t.Fatalf("through = %d", prior.ThroughPosition)
	}
	if len(prior.RetainedPositions) != 0 {
		t.Fatalf("retained = %v", prior.RetainedPositions)
	}
}

// Late terminal output can mutate a prior tool without updating its row
// timestamp.
func TestTerminalAndControllerPlanAreRetainedWithoutReplayingTheWholeTurn(t *testing.T) {
	canonical := claudeSnapshot([]core.CanonicalTranscriptBody{
		claudeUser("work"),
		claudeTool("tool-a", true),
		core.CanonicalTranscriptBodyPlan{Plan: json.RawMessage(`{"entries":[]}`)},
		claudeAgent("late update"),
		claudeTool("tool-b", false),
	})
	ordinal := uint64(10)
	canonical.Transcript[3].LatestContentEventOrdinal = &ordinal
	pair, records := claudeNative([]nativeRecord{
		nativeUser("work"),
		nativeToolUse("tool-a"),
		nativeToolResult("tool-a"),
		nativeToolUse("tool-b"),
		nativeToolResult("tool-b"),
	})
	prior := mapCompaction(pair, records, nil, canonical)
	if prior == nil || prior.ThroughPosition != 4 || !slices.Equal(prior.RetainedPositions, []uint64{2, 3, 4}) {
		t.Fatalf("prior = %+v", prior)
	}
	canonical.Transcript[1].Body = claudeTool("tool-a", false)
	prior = mapCompaction(pair, records, nil, canonical)
	if prior == nil || prior.ThroughPosition != 4 || !slices.Equal(prior.RetainedPositions, []uint64{3, 4}) {
		t.Fatalf("prior = %+v", prior)
	}
}

func TestRepeatedToolIDAfterBoundaryIsNotASafeAnchor(t *testing.T) {
	canonical := claudeSnapshot([]core.CanonicalTranscriptBody{claudeUser("work"), claudeTool("tool-a", false)})
	pair, records := claudeNative([]nativeRecord{
		nativeUser("work"),
		nativeToolUse("tool-a"),
		nativeToolResult("tool-a"),
	})
	following := []nativeRecord{nativeToolUse("tool-a")}
	if prior := mapCompaction(pair, records, following, canonical); prior != nil {
		t.Fatalf("prior = %+v", prior)
	}
}

func TestUnrepresentedUserIsReplayedBeforeTheToolBarrier(t *testing.T) {
	canonical := claudeSnapshot([]core.CanonicalTranscriptBody{claudeUser("first"), claudeUser("second"), claudeTool("tool-a", false)})
	pair, records := claudeNative([]nativeRecord{
		nativeUser("first"),
		nativeToolUse("tool-a"),
		nativeToolResult("tool-a"),
	})
	prior := mapCompaction(pair, records, nil, canonical)
	if prior == nil || prior.ThroughPosition != 2 || !slices.Equal(prior.RetainedPositions, []uint64{2}) {
		t.Fatalf("prior = %+v", prior)
	}
}

func TestCurrentContextBoundaryLimitsNativeAnchorSearch(t *testing.T) {
	canonical := claudeSnapshot([]core.CanonicalTranscriptBody{
		claudeUser("duplicate"),
		core.CanonicalTranscriptBodySystem{Text: "context cleared"},
		claudeUser("duplicate"),
		claudeTool("tool-a", false),
	})
	canonical.Transcript[1].StableID = "context-cleared:2"
	pair, records := claudeNative([]nativeRecord{
		nativeUser("duplicate"),
		nativeToolUse("tool-a"),
		nativeToolResult("tool-a"),
	})
	prior := mapCompaction(pair, records, nil, canonical)
	if prior == nil || prior.ThroughPosition != 3 {
		t.Fatalf("prior = %+v", prior)
	}
}

func TestValidatedHiddenHandoffBoundsNativeUserLineage(t *testing.T) {
	beforeHandoff := claudeSnapshot([]core.CanonicalTranscriptBody{claudeUser("old harness request")})
	beforeHandoff.EventFrontier = 1
	state, err := transcript.HandoffStateCapture(beforeHandoff, "generic earlier state")
	if err != nil {
		t.Fatal(err)
	}
	canonical := claudeSnapshot([]core.CanonicalTranscriptBody{
		claudeUser("old harness request"),
		claudeUser("new request"),
		claudeTool("tool-a", false),
	})
	encoded, err := json.Marshal(state)
	if err != nil {
		t.Fatal(err)
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	canonical.Session.Configuration.Values = map[string]any{transcript.HandoffStateKey: value}
	pair, records := claudeNative([]nativeRecord{
		nativeUser("new request"),
		nativeToolUse("tool-a"),
		nativeToolResult("tool-a"),
	})
	prior := mapCompaction(pair, records, nil, canonical)
	if prior == nil || prior.ThroughPosition != 2 {
		t.Fatalf("prior = %+v", prior)
	}
}

func TestVerifiedClaudeCheckpointSuppliesSeedAndRetainedPlanToCompactionPrompt(t *testing.T) {
	canonical := claudeSnapshot([]core.CanonicalTranscriptBody{
		claudeUser("work"),
		core.CanonicalTranscriptBodySystem{Text: "startup notice"},
		core.CanonicalTranscriptBodyThought{Chunks: []json.RawMessage{json.RawMessage(`{"content":{"type":"text","text":"private thought"}}`)}},
		claudeAgent("old agent text"),
		claudeTool("tool-a", false),
		core.CanonicalTranscriptBodyPlan{Plan: json.RawMessage(`{"entries":[{"content":"keep plan","priority":"high","status":"in_progress"}]}`)},
		claudeTool("tool-b", false),
		claudeAgent("new agent text"),
	})
	four, eight := uint64(4), uint64(8)
	canonical.Transcript[3].LatestContentEventOrdinal = &four
	canonical.Transcript[7].LatestContentEventOrdinal = &eight
	native := claudeLines(
		`{"message":{"content":"work"},"type":"user"}`,
		`{"message":{"content":[{"id":"tool-a","type":"tool_use"}]},"type":"assistant"}`,
		`{"message":{"content":[{"tool_use_id":"tool-a","type":"tool_result"}]},"type":"user"}`,
		`{"message":{"content":[{"id":"tool-b","type":"tool_use"}]},"type":"assistant"}`,
		`{"message":{"content":[{"tool_use_id":"tool-b","type":"tool_result"}]},"type":"user"}`,
		`{"subtype":"compact_boundary","timestamp":"2026-01-01T00:00:00Z","type":"system"}`,
		`{"isCompactSummary":true,"message":{"content":"native prior state"},"timestamp":"2026-01-01T00:00:01Z","type":"user"}`,
	)
	input := checkpoint.ArchiveInput{
		Session: core.SessionManifest{
			ID:              "session-1",
			Title:           "Work",
			HarnessKind:     core.HarnessKindClaude,
			ProfileID:       "claude",
			NativeSessionID: "native-1",
			CreatedAt:       "2026-01-01T00:00:00Z",
			CheckpointedAt:  "2026-01-01T00:01:00Z",
			HelVersion:      "test",
			RelayVersion:    "test",
			AdapterVersion:  "test",
		},
		Target: core.TargetManifest{
			TemplateID: "test",
			TargetKind: "bare",
			Details:    map[string]string{},
		},
		Bundle: core.BundleManifest{
			ID:                "bundle",
			PrimaryRepository: "repo",
		},
		CanonicalSession: *canonical,
		NativeArtifacts: []core.NativeArtifact{{
			RelativePath: "projects/-work/native-1.jsonl",
			Data:         core.NativeArtifactDataBytes(native),
			Mode:         0o600,
		}},
	}
	path := filepath.Join(t.TempDir(), "checkpoint.zip")
	if _, err := checkpoint.WriteArchiveAtomic(path, &input); err != nil {
		t.Fatal(err)
	}
	verified, err := checkpoint.VerifyArchiveStreaming(path)
	if err != nil {
		t.Fatal(err)
	}
	prior, err := priorCompaction(context.Background(), path, &verified.Manifest, &verified.CanonicalSession)
	if err != nil {
		t.Fatal(err)
	}
	if prior == nil || prior.ThroughPosition != 6 || !slices.Equal(prior.RetainedPositions, []uint64{6}) {
		t.Fatalf("prior = %+v", prior)
	}
	backend := &claudeRecordingBackend{}
	if _, err := CompactSnapshotSince(context.Background(), &verified.CanonicalSession, CompactionBudgetUniform(64*1024), backend, prior); err != nil {
		t.Fatal(err)
	}
	backend.mu.Lock()
	defer backend.mu.Unlock()
	if len(backend.prompts) != 1 {
		t.Fatalf("prompts = %d", len(backend.prompts))
	}
	prompt := backend.prompts[0]
	for _, kept := range []string{"native prior state", "keep plan", "tool-b"} {
		if !strings.Contains(prompt, kept) {
			t.Fatalf("prompt lacks %q: %s", kept, prompt)
		}
	}
	for _, dropped := range []string{"old agent text", "private thought"} {
		if strings.Contains(prompt, dropped) {
			t.Fatalf("prompt has %q: %s", dropped, prompt)
		}
	}
}
