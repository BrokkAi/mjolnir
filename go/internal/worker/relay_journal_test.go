// Port of mj-worker/src/relay/journal.rs (its #[cfg(test)] items) and of
// mj-worker/src/relay/journal/tests.rs.

package worker

import (
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func (r *DurableRelay) retainedEventBodyCount() int {
	return len(r.hotEvents)
}

// setSealByteLimitOverride sets (or, with nil, clears) the active-segment seal
// threshold. Rust scopes it to the current test thread; see
// sealByteLimitOverride.
func setSealByteLimitOverride(limit *uint64) {
	sealByteLimitOverride.Store(limit)
}

// --- helpers for the journal tests ---

func journalTestOpen(t *testing.T, root, sessionID, version string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(root, sessionID, version)
	if err != nil {
		t.Fatalf("open relay at %s: %v", root, err)
	}
	// The test owns the relay, so it shuts it down: registered after the
	// t.TempDir that made root, this runs before the removal.
	t.Cleanup(relay.Shutdown)
	return relay
}

func journalTestRecord(t *testing.T, relay *DurableRelay, observation core.RelayObservation) uint64 {
	t.Helper()
	ordinal, err := relay.RecordObservation(observation)
	if err != nil {
		t.Fatalf("record %T: %v", observation, err)
	}
	return ordinal
}

func journalTestWarning(t *testing.T, relay *DurableRelay, message string) uint64 {
	t.Helper()
	return journalTestRecord(t, relay, core.RelayObservationWarning{Message: message})
}

func journalTestClaim(t *testing.T, relay *DurableRelay) []core.ClaimedRelayCommand {
	t.Helper()
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		t.Fatalf("claim pending commands: %v", err)
	}
	return claimed
}

func journalTestEndTurn() core.RelayCommandOutcomePrompt {
	return core.RelayCommandOutcomePrompt{StopReason: "end_turn"}
}

func journalTestComplete(t *testing.T, relay *DurableRelay, commandID string, outcome core.RelayCommandOutcome) {
	t.Helper()
	if _, err := relay.RecordCommandCompleted(commandID, outcome); err != nil {
		t.Fatalf("complete %s: %v", commandID, err)
	}
}

func journalTestTextChunk(text string) acpschema.SessionUpdate {
	return acpschema.SessionUpdate{
		Kind: acpschema.SessionUpdateKindAgentMessageChunk,
		AgentMessageChunk: &acpschema.ContentChunk{Content: acpschema.ContentBlock{
			Kind: acpschema.ContentBlockKindText,
			Text: &acpschema.TextContent{Text: text},
		}},
	}
}

func journalTestTextBlock(text string) acpschema.ContentBlock {
	return acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}
}

func journalTestSessionUpdate(t *testing.T, relay *DurableRelay, update acpschema.SessionUpdate) {
	t.Helper()
	if _, err := relay.RecordSessionUpdate(update); err != nil {
		t.Fatalf("record session update: %v", err)
	}
}

func journalTestSessionConfigured(t *testing.T, relay *DurableRelay) {
	t.Helper()
	journalTestRecord(t, relay, core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
}

func journalTestAppend(t *testing.T, relay *DurableRelay, commandID string, observation core.RelayObservation) {
	t.Helper()
	if _, err := relay.appendRelayEvent(&commandID, observation); err != nil {
		t.Fatalf("append %T: %v", observation, err)
	}
}

func journalTestAcknowledged(body core.RelayResponseBody) (core.RelayResponsePayloadAcknowledged, bool) {
	ok, isOk := body.(core.RelayResponseBodyOk)
	if !isOk {
		return core.RelayResponsePayloadAcknowledged{}, false
	}
	acknowledged, isAcknowledged := ok.Payload.(core.RelayResponsePayloadAcknowledged)
	return acknowledged, isAcknowledged
}

func journalTestProtocolError(body core.RelayResponseBody) (core.RelayProtocolError, bool) {
	failed, isError := body.(core.RelayResponseBodyError)
	return failed.Error, isError
}

func journalTestErrorCode(body core.RelayResponseBody, code core.RelayErrorCode) bool {
	failed, isError := journalTestProtocolError(body)
	return isError && failed.Code == code
}

func journalTestEventsAfter(t *testing.T, relay *DurableRelay, ordinal uint64, digest string) []core.RelayEvent {
	t.Helper()
	events, err := relay.EventsAfter(ordinal, digest)
	if err != nil {
		t.Fatalf("events after %d: %v", ordinal, err)
	}
	return events
}

func journalTestJSON(t *testing.T, value any) string {
	t.Helper()
	encoded, err := workerRuntimeMarshalJSON(value)
	if err != nil {
		t.Fatalf("encode %T: %v", value, err)
	}
	return string(encoded)
}

func journalTestPath(root string) (journal, active string) {
	journal = filepath.Join(root, core.RelayJournalDir)
	return journal, filepath.Join(journal, core.RelayActiveSegment)
}

func journalTestFileSize(t *testing.T, path string) int64 {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Size()
}

func journalTestReadFile(t *testing.T, path string) []byte {
	t.Helper()
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return body
}

func journalTestWriteFile(t *testing.T, path string, body []byte) {
	t.Helper()
	if err := os.WriteFile(path, body, 0o644); err != nil {
		t.Fatal(err)
	}
}

func journalTestGzFiles(t *testing.T, journal string) []string {
	t.Helper()
	entries, err := os.ReadDir(journal)
	if err != nil {
		t.Fatal(err)
	}
	var sealed []string
	for _, entry := range entries {
		path := filepath.Join(journal, entry.Name())
		if relayJournalIsGz(path) {
			sealed = append(sealed, path)
		}
	}
	slices.Sort(sealed)
	return sealed
}

// journalTestBreakState replaces relay-state.json with a directory, so every
// later snapshot write fails.
func journalTestBreakState(t *testing.T, root string) string {
	t.Helper()
	statePath := filepath.Join(root, core.RelayStateFile)
	if err := os.Remove(statePath); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(statePath, 0o755); err != nil {
		t.Fatal(err)
	}
	return statePath
}

func journalTestSignedEvent(t *testing.T, event core.RelayEvent) core.RelayEvent {
	t.Helper()
	digest, err := core.RelayEventDigest(&event)
	if err != nil {
		t.Fatal(err)
	}
	event.Digest = digest
	return event
}

func journalTestWriteEvent(t *testing.T, file *os.File, event core.RelayEvent) {
	t.Helper()
	encoded := journalTestJSON(t, event)
	if _, err := file.WriteString(encoded + "\n"); err != nil {
		t.Fatal(err)
	}
}

func journalTestVisit(path string, mode JournalReadMode, seen *[]uint64) ([]RelayJournalGap, error) {
	return visitRelayJournalFile(path, mode, func(event core.RelayEvent, _ int) (bool, error) {
		*seen = append(*seen, event.Ordinal)
		return false, nil
	})
}

func journalTestHasRestartContinuation(relay *DurableRelay) bool {
	for id := range relay.snapshot.HandledCommands {
		if core.IsRestartContinuation(id) {
			return true
		}
	}
	return false
}

func journalTestAnyEvent(relay *DurableRelay, matches func(observation core.RelayObservation) bool) bool {
	return slices.ContainsFunc(retainedEvents(relay), func(event core.RelayEvent) bool {
		return matches(event.Observation)
	})
}

func journalTestBlocksForCommand(t *testing.T, claimed core.ClaimedRelayCommand) []acpschema.ContentBlock {
	t.Helper()
	blocks, ok := core.RelayCommandPromptBlocksForCommand(claimed.Command, claimed.CommandID)
	if !ok {
		t.Fatalf("command %s has no prompt blocks: %#v", claimed.CommandID, claimed.Command)
	}
	return blocks
}

func journalTestPromptBlocks(t *testing.T, command core.RelayCommand) []acpschema.ContentBlock {
	t.Helper()
	blocks, ok := core.RelayCommandPromptBlocks(command)
	if !ok {
		t.Fatalf("command has no prompt blocks: %#v", command)
	}
	return blocks
}

// --- tests ---

// Hard-won: b62fe3b: later notices evicted the turn facts still read by the process watcher.
func TestCurrentTurnWorkReceiptSurvivesAcknowledgedControlFloodAndRestart(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "new")
	submitRelay(relay, "watched-turn", prompt("start background work"))
	journalTestClaim(t, relay)
	process := core.ProcessIdentity{Pid: 4242, Start: "same-process"}
	journalTestComplete(t, relay, "watched-turn", core.RelayCommandOutcomePrompt{
		StopReason: "end_turn",
		WorkFacts: &core.WorkFacts{
			ObservedAtMs: 77,
			Processes: core.ObservedKnown(core.ProcessFacts{
				Running:      1,
				Shown:        []core.RunningProcess{},
				Omitted:      1,
				Identities:   []core.ProcessIdentity{process},
				Unattributed: 0,
			}),
			Worktree: core.ObservedUnavailable[core.WorktreeFacts]("not inspected"),
		},
	})
	for index := 0; index <= core.RelayRetryIDRetention; index++ {
		submitRelay(relay, fmt.Sprintf("work-notice-%d", index), core.RelayCommandRecordNotice{Text: "later notice"})
	}
	through := relay.LatestOrdinal()
	acknowledgeRelay(relay, "ack-work-notices", through)
	if _, found := relay.snapshot.HandledCommands["work-notice-0"]; found {
		t.Fatal("the oldest notice receipt should have been pruned")
	}

	relay = journalTestOpen(t, temp, session, "new")
	_, identities, _ := relay.watchedWork()
	if !reflect.DeepEqual(identities, []core.ProcessIdentity{process}) {
		t.Fatalf("watched processes = %+v", identities)
	}
}

// Hard-won: 806b123: a crash during ACK cleanup left sealed history below the durable cut.
func TestAcknowledgedWorkerReopensAfterInterruptedSealedSegmentCleanup(t *testing.T) {
	for _, replacementInstalled := range []bool{false, true} {
		temp := t.TempDir()
		_, active := journalTestPath(temp)
		relay := journalTestOpen(t, temp, session, "new")
		for range 40 {
			journalTestWarning(t, relay, strings.Repeat("x", 64*1024))
		}
		oldSegment := relay.journalSpans[0].path
		if filepath.Ext(oldSegment) != ".gz" {
			t.Fatalf("oldest span %s is not sealed", oldSegment)
		}
		leftover := journalTestReadFile(t, oldSegment)
		oldActive := journalTestReadFile(t, active)
		frontier := relay.LatestOrdinal()
		digest := relay.LatestDigest()
		response := acknowledgeRelay(relay, "collect-before-crash", frontier)
		if _, ok := journalTestAcknowledged(response.Body); !ok {
			t.Fatalf("acknowledgement failed: %#v", response.Body)
		}
		if size := journalTestFileSize(t, active); size != 0 {
			t.Fatalf("active segment has %d bytes after collection", size)
		}
		// The snapshot and empty active replacement are durable, but deletion
		// of this already covered sealed file did not finish before the
		// worker died.
		journalTestWriteFile(t, oldSegment, leftover)
		if !replacementInstalled {
			journalTestWriteFile(t, active, oldActive)
		}
		relay = journalTestOpen(t, temp, session, "new")
		if relay.LatestOrdinal() != frontier || relay.LatestDigest() != digest {
			t.Fatalf("reopened at %d/%s, want %d/%s", relay.LatestOrdinal(), relay.LatestDigest(), frontier, digest)
		}
		retried := acknowledgeRelay(relay, "retry-identical-ack", frontier)
		if _, ok := journalTestAcknowledged(retried.Body); !ok {
			t.Fatalf("retried acknowledgement failed: %#v", retried.Body)
		}
		if relayPathExists(oldSegment) {
			t.Fatal("the same ACK must remove physical leftovers after reopen")
		}
		if size := journalTestFileSize(t, active); size != 0 {
			t.Fatalf("active segment has %d bytes after the retried collection", size)
		}
		journalTestWarning(t, relay, "after restart")
		if events := journalTestEventsAfter(t, relay, frontier, digest); events[0].Ordinal != frontier+1 {
			t.Fatalf("first event after the frontier is %d", events[0].Ordinal)
		}
	}
}

// Hard-won: 581ae9e: catch-up ACK decompressed damaged segments already covered by its cursor.
func TestAcknowledgementDiscardsUnreadableCoveredSegmentsWithoutReadingThem(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "new")
	for range 80 {
		journalTestWarning(t, relay, strings.Repeat("x", 64*1024))
	}
	covered := relay.journalSpans[0]
	if filepath.Ext(covered.path) != ".gz" {
		t.Fatalf("oldest span %s is not sealed", covered.path)
	}
	through := relay.LatestOrdinal() - 1
	digest, found, err := relay.digestAt(through)
	if err != nil || !found {
		t.Fatalf("digest at %d: %v %v", through, found, err)
	}
	journalTestWriteFile(t, covered.path, []byte("damaged compressed history"))
	response := relay.Handle(relayRequest("ack-damaged-prefix", core.RelayRequestAcknowledge{
		ThroughOrdinal: through,
		ThroughDigest:  digest,
	}))
	if _, ok := journalTestAcknowledged(response.Body); !ok {
		t.Fatalf("%#v", response)
	}

	relay = journalTestOpen(t, temp, session, "new")
	events := journalTestEventsAfter(t, relay, through, digest)
	if events[0].Ordinal != through+1 || events[len(events)-1].Ordinal != 80 {
		t.Fatalf("events span %d..%d", events[0].Ordinal, events[len(events)-1].Ordinal)
	}
}

// TestStreamedChunkAppendCost measures streamed chunk cost both ways in one
// process so a loaded machine cannot flatter either policy. Run with
// `go test -run TestStreamedChunkAppendCost -v` and MJ_RUN_TIMING_TESTS=1.
func TestStreamedChunkAppendCost(t *testing.T) {
	if os.Getenv("MJ_RUN_TIMING_TESTS") == "" {
		t.Skip("timing measurement, not a behavior assertion")
	}
	const backlog = 40
	const chunks = 200
	const rounds = 3

	streamChunks := func(stageEveryAppend bool) (time.Duration, int) {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "1.0.0")
		// Give the snapshot the weight a real session carries: a queue of
		// prompts the checkpoint has not pruned yet.
		for index := range backlog {
			submitRelay(relay, fmt.Sprintf("backlog-command-%04d", index), prompt(strings.Repeat("q", 4096)))
		}
		snapshotBytes := len(journalTestReadFile(t, filepath.Join(temp, core.RelayStateFile)))
		relay.stageSnapshotEveryAppend = stageEveryAppend

		chunk := strings.Repeat("token ", 40)
		started := time.Now()
		for range chunks {
			journalTestSessionUpdate(t, relay, journalTestTextChunk(chunk))
		}
		return time.Since(started), snapshotBytes
	}

	var amortized, everyAppend time.Duration
	snapshotBytes := 0
	for range rounds {
		elapsed, size := streamChunks(false)
		amortized += elapsed
		snapshotBytes = size
		elapsed, _ = streamChunks(true)
		everyAppend += elapsed
	}

	// What one redundant snapshot write costs: the collection that follows an
	// advancing acknowledgement used to pay exactly this.
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	for index := range backlog {
		submitRelay(relay, fmt.Sprintf("backlog-command-%04d", index), prompt(strings.Repeat("q", 4096)))
	}
	started := time.Now()
	for range chunks {
		if err := relay.persistSnapshot(); err != nil {
			t.Fatal(err)
		}
	}
	persists := time.Since(started)

	appends := time.Duration(chunks * rounds)
	t.Logf("snapshot %d bytes, %d chunk appends per policy\n  "+
		"snapshot per append: %v (%v/append)\n  "+
		"amortized:           %v (%v/append)\n  "+
		"one snapshot write:  %v",
		snapshotBytes, chunks*rounds, everyAppend, everyAppend/appends, amortized, amortized/appends, persists/chunks)
}

func persistedRelaySnapshot(t *testing.T, root string) core.RelaySnapshot {
	t.Helper()
	var snapshot core.RelaySnapshot
	if err := json.Unmarshal(journalTestReadFile(t, filepath.Join(root, core.RelayStateFile)), &snapshot); err != nil {
		t.Fatal(err)
	}
	return snapshot
}

// journalTestSnapshotValue is serde_json::to_value of a fresh snapshot, as an
// order-preserving JSON object.
func journalTestSnapshotValue(t *testing.T) jsonvalue.Object {
	t.Helper()
	fresh := core.NewRelaySnapshot(session)
	var value jsonvalue.Object
	if err := json.Unmarshal([]byte(journalTestJSON(t, fresh)), &value); err != nil {
		t.Fatal(err)
	}
	return value
}

func TestRevision16SnapshotAndJournalReplayLegacyMailboxText(t *testing.T) {
	snapshotRoot := t.TempDir()
	snapshot := journalTestSnapshotValue(t)
	snapshot.Set("format_version", json.Number("16"))
	snapshot.Set("pending_mailbox_events", []any{jsonvalue.ObjectOf(map[string]any{
		"key":           "legacy:snapshot",
		"source":        "github",
		"wake":          false,
		"text":          "event from revision-16 snapshot",
		"created_at_ms": json.Number("10"),
	})})
	journalTestWriteFile(t, filepath.Join(snapshotRoot, core.RelayStateFile), []byte(journalTestJSON(t, snapshot)))
	snapshotRelay := journalTestOpen(t, snapshotRoot, session, "test")
	if snapshotRelay.snapshot.FormatVersion != core.RelayStateVersion {
		t.Fatalf("format version %d", snapshotRelay.snapshot.FormatVersion)
	}
	if body, ok := snapshotRelay.snapshot.PendingMailboxEvents[0].Body.(core.MailboxEventBodyPlainText); !ok ||
		body.Text != "event from revision-16 snapshot" {
		t.Fatalf("snapshot mailbox body = %#v", snapshotRelay.snapshot.PendingMailboxEvents[0].Body)
	}

	// The revision-16 wire shapes, written by hand as serde wrote them.
	legacyEvent := `{"key":"legacy:journal","source":"github","wake":false,"text":"event from revision-16 journal","created_at_ms":20}`
	observation := `{"type":"command_queued","data":{"command_id":"legacy-journal-command","command":{"type":"deliver_mailbox_event","data":{"event":` +
		legacyEvent + `}},"created_at_ms":20}}`
	payload := `{"ordinal":1,"recorded_at_ms":20,"command_id":"legacy-journal-command","observation":` + observation + `}`

	journalRoot := t.TempDir()
	journal, active := journalTestPath(journalRoot)
	if err := os.MkdirAll(journal, 0o755); err != nil {
		t.Fatal(err)
	}
	revision16 := journalTestSnapshotValue(t)
	revision16.Set("format_version", json.Number("16"))
	journalTestWriteFile(t, filepath.Join(journalRoot, core.RelayStateFile), []byte(journalTestJSON(t, revision16)))

	digest := sha256.New()
	digest.Write(core.RelayEventDigestDomainV2)
	digest.Write([]byte(payload))
	record := `{"format":2,"ordinal":1,"digest":"` + core.LowerHex(digest.Sum(nil)) +
		`","recorded_at_ms":20,"command_id":"legacy-journal-command","observation":` + observation + "}"
	journalTestWriteFile(t, active, []byte(record+"\n"))

	journalRelay := journalTestOpen(t, journalRoot, session, "test")
	if journalRelay.LatestOrdinal() < 1 {
		t.Fatalf("latest ordinal %d", journalRelay.LatestOrdinal())
	}
	if journalRelay.snapshot.FormatVersion != core.RelayStateVersion {
		t.Fatalf("format version %d", journalRelay.snapshot.FormatVersion)
	}
	if !slices.ContainsFunc(journalRelay.snapshot.PendingMailboxEvents, func(event core.MailboxEvent) bool {
		body, ok := event.Body.(core.MailboxEventBodyPlainText)
		return event.Key == "legacy:journal" && ok && body.Text == "event from revision-16 journal"
	}) {
		t.Fatalf("pending mailbox events = %+v", journalRelay.snapshot.PendingMailboxEvents)
	}
}

// A relay that dies mid-turn keeps every chunk it acknowledged, and the relay
// that reopens republishes the frontier it replayed.
// Hard-won: ce467bc6f: restoring tails closed an open reply and lost its prefix on the next chunk.
func TestARelayThatDiesMidStreamRecoversItsUnpersistedChunks(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	// The prompt stays promoted but unclaimed, so recovery has no in-flight
	// ACP command to interrupt and the frontier is exactly what was
	// journaled.
	submitRelay(relay, "interrupted-prompt", prompt("stream"))
	for index := range 8 {
		journalTestSessionUpdate(t, relay, journalTestTextChunk(fmt.Sprintf("chunk %d", index)))
	}
	frontier := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	// No teardown: the process is gone before anything else is written.

	reopened := journalTestOpen(t, temp, session, "1.0.0")
	if reopened.LatestOrdinal() != frontier || reopened.LatestDigest() != digest {
		t.Fatalf("reopened at %d, want %d", reopened.LatestOrdinal(), frontier)
	}
	if events := journalTestEventsAfter(t, reopened, 0, core.RelayEventGenesisDigest); uint64(len(events)) != frontier {
		t.Fatalf("%d events retained, want %d", len(events), frontier)
	}
	if latest := persistedRelaySnapshot(t, temp).LatestOrdinal; latest != frontier {
		t.Fatalf("recovery must republish the frontier it replayed: %d != %d", latest, frontier)
	}
	journalTestSessionUpdate(t, reopened, journalTestTextChunk("chunk 8"))
	facts := core.ActivityFacts{}
	evidence := reopened.TurnContext().Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &facts, 0)
	var expected strings.Builder
	for index := range 9 {
		fmt.Fprintf(&expected, "chunk %d", index)
	}
	if evidence.AssistantTextTail != expected.String() {
		t.Fatalf("assistant tail = %q", evidence.AssistantTextTail)
	}
}

// A catch-up acknowledgement wrote relay-state.json twice: once for the ACK,
// then again for a collection that had nothing to collect.
func TestAnAcknowledgementWithNothingToCollectDoesNotRewriteTheSnapshot(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	// Pin retained history at a verified checkpoint and let the collection
	// that follows it finish.
	ready := readyCheckpoint(relay, "catch-up-barrier")
	acknowledgeRelay(relay, "ack-catch-up-barrier", ready.Ordinal)
	submitRelay(relay, "complete-catch-up", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "catch-up-barrier"})
	journalTestWarning(t, relay, "streamed past the floor")
	through := relay.LatestOrdinal()
	acknowledgeRelay(relay, "ack-catch-up", through)

	// From here any snapshot write fails, so a redundant one is visible.
	journalTestBreakState(t, temp)
	repeated := acknowledgeRelay(relay, "ack-catch-up-again", through)
	if acknowledged, ok := journalTestAcknowledged(repeated.Body); !ok || acknowledged.ThroughOrdinal != through {
		t.Fatalf("history collection rewrote an unchanged snapshot: %#v", repeated.Body)
	}
}

func TestRestartFinishesADurablyAcceptedQueueRemoval(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	submitRelay(relay, "active-prompt", prompt("active"))
	submitRelay(relay, "queued-prompt", prompt("remove me"))
	journalTestAppend(t, relay, "remove-after-crash", core.RelayObservationCommandQueued{
		CommandID:   "remove-after-crash",
		Command:     core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: "queued-prompt"},
		CreatedAtMs: core.EpochMillis(),
	})

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if queued := relay.OperationalState().QueuedPrompts; len(queued) != 0 {
		t.Fatalf("queued prompts = %+v", queued)
	}
	if !journalTestAnyEvent(relay, func(observation core.RelayObservation) bool {
		completed, ok := observation.(core.RelayObservationCommandCompleted)
		if !ok || completed.CommandID != "remove-after-crash" {
			return false
		}
		changed, ok := completed.Outcome.(core.RelayCommandOutcomeQueueChanged)
		return ok && slices.Equal(changed.RemovedCommandIds, []string{"queued-prompt"})
	}) {
		t.Fatal("the queue removal was not finished")
	}
}

func TestRestartFinishesADurablyAcceptedQueueClear(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	submitRelay(relay, "active-prompt", prompt("active"))
	submitRelay(relay, "queued-one", prompt("one"))
	submitRelay(relay, "queued-two", prompt("two"))
	journalTestAppend(t, relay, "clear-after-crash", core.RelayObservationCommandQueued{
		CommandID:   "clear-after-crash",
		Command:     core.RelayCommandClearQueuedPrompts{},
		CreatedAtMs: core.EpochMillis(),
	})

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if queued := relay.OperationalState().QueuedPrompts; len(queued) != 0 {
		t.Fatalf("queued prompts = %+v", queued)
	}
	if !journalTestAnyEvent(relay, func(observation core.RelayObservation) bool {
		completed, ok := observation.(core.RelayObservationCommandCompleted)
		if !ok || completed.CommandID != "clear-after-crash" {
			return false
		}
		changed, ok := completed.Outcome.(core.RelayCommandOutcomeQueueChanged)
		return ok && slices.Equal(changed.RemovedCommandIds, []string{"queued-one", "queued-two"})
	}) {
		t.Fatal("the queue clear was not finished")
	}
}

func TestRestartFinishesCheckpointCompletionBeforeReleasingOwnerlessBarriers(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	ready := readyCheckpoint(relay, "barrier-command")
	journalTestAppend(t, relay, "complete-after-crash", core.RelayObservationCommandQueued{
		CommandID:   "complete-after-crash",
		Command:     core.RelayCommandCompleteCheckpoint{BarrierCommandID: "barrier-command"},
		CreatedAtMs: core.EpochMillis(),
	})

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if relay.snapshot.RecoveryFloorOrdinal != ready.Ordinal {
		t.Fatalf("recovery floor %d, want %d", relay.snapshot.RecoveryFloorOrdinal, ready.Ordinal)
	}
	if relay.snapshot.CheckpointBarrier != nil {
		t.Fatalf("checkpoint barrier %q survived", *relay.snapshot.CheckpointBarrier)
	}
	if journalTestAnyEvent(relay, func(observation core.RelayObservation) bool {
		interrupted, ok := observation.(core.RelayObservationCommandInterrupted)
		return ok && interrupted.CommandID == "barrier-command"
	}) {
		t.Fatal("the completed barrier was interrupted")
	}
}

func TestRestartInterruptsOwnerlessCheckpointButPreservesAcceptedClose(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	expected := readyCheckpoint(relay, "close-barrier")
	submitRelay(relay, "accepted-close", core.RelayCommandClose{BarrierCommandID: "close-barrier", Expected: expected})

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if !journalTestAnyEvent(relay, func(observation core.RelayObservation) bool {
		interrupted, ok := observation.(core.RelayObservationCommandInterrupted)
		return ok && interrupted.CommandID == "close-barrier" &&
			interrupted.Command == core.RelayCommandKindBeginCheckpoint &&
			interrupted.Reason != nil && *interrupted.Reason == core.OutcomeReasonOwnerLostOnRestart
	}) {
		t.Fatal("the ownerless barrier was not interrupted")
	}
	if execution := relay.OperationalState().Execution; execution != core.RelayExecutionStateClosing {
		t.Fatalf("execution = %v", execution)
	}
	claimed := journalTestClaim(t, relay)
	if len(claimed) != 1 || claimed[0].CommandID != "accepted-close" {
		t.Fatalf("claimed = %+v", claimed)
	}
	if _, closing := claimed[0].Command.(core.RelayCommandClose); !closing {
		t.Fatalf("claimed command = %#v", claimed[0].Command)
	}
}

func TestRelayCommandSubmissionIsIdempotentAcrossRestart(t *testing.T) {
	temp := t.TempDir()
	firstOrdinal := submitRelay(journalTestOpen(t, temp, session, "1.0.0"), "stable-command", prompt("once"))
	relay := journalTestOpen(t, temp, session, "1.0.0")
	if repeated := submitRelay(relay, "stable-command", prompt("once")); repeated != firstOrdinal {
		t.Fatalf("repeated submit accepted at %d, want %d", repeated, firstOrdinal)
	}
	queued := 0
	for _, event := range retainedEvents(relay) {
		if observed, ok := event.Observation.(core.RelayObservationCommandQueued); ok && observed.CommandID == "stable-command" {
			queued++
		}
	}
	if queued != 1 {
		t.Fatalf("stable-command queued %d times", queued)
	}

	response := relay.Handle(relayRequest("request-conflict", core.RelayRequestSubmit{
		CommandID: "stable-command",
		Command:   prompt("different"),
	}))
	if !journalTestErrorCode(response.Body, core.RelayErrorCodeInvalidRequest) {
		t.Fatalf("conflicting submit = %#v", response.Body)
	}
}

func TestCommandIdempotencySurvivesAckUntilCheckpointCoversTerminalEvent(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	accepted := submitRelay(relay, "checkpointed-command", prompt("once"))
	if claimed := journalTestClaim(t, relay); claimed[0].CommandID != "checkpointed-command" {
		t.Fatalf("claimed %s", claimed[0].CommandID)
	}
	journalTestComplete(t, relay, "checkpointed-command", journalTestEndTurn())
	terminal := relay.LatestOrdinal()
	attachRelay(relay, "attach-idempotency", 0)
	acknowledgeRelay(relay, "ack-idempotency", terminal)
	if repeated := submitRelay(relay, "checkpointed-command", prompt("once")); repeated != accepted {
		t.Fatalf("ACK must not prune the stable command ID: %d != %d", repeated, accepted)
	}

	coverWithCheckpoint(relay, "idempotency-barrier")
	if repeated := submitRelay(relay, "checkpointed-command", prompt("once")); repeated != accepted {
		t.Fatalf("a checkpoint alone must not forget a recent command ID: %d != %d", repeated, accepted)
	}
}

// completePrompt submits, dispatches and finishes one prompt, returning its
// acceptance ordinal.
func completePrompt(relay *DurableRelay, commandID string) uint64 {
	accepted := submitRelay(relay, commandID, prompt(commandID))
	claimed, err := relay.ClaimPendingCommands(true)
	if err != nil {
		panic(err)
	}
	if claimed[0].CommandID != commandID {
		panic(fmt.Sprintf("claimed %s, expected %s", claimed[0].CommandID, commandID))
	}
	if _, err := relay.RecordCommandCompleted(commandID, journalTestEndTurn()); err != nil {
		panic(err)
	}
	return accepted
}

// coverWithCheckpoint acknowledges everything and completes a checkpoint that
// covers it, which is what lets journal collection prune below the new
// frontier.
func coverWithCheckpoint(relay *DurableRelay, barrier string) {
	latest := relay.LatestOrdinal()
	attachRelay(relay, "attach-"+barrier, 0)
	acknowledgeRelay(relay, "ack-"+barrier, latest)
	ready := readyCheckpoint(relay, barrier)
	acknowledgeRelay(relay, "ack-ready-"+barrier, ready.Ordinal)
	submitRelay(relay, "complete-"+barrier, core.RelayCommandCompleteCheckpoint{BarrierCommandID: barrier})
}

// Hard-won: 50a0277d2: retained terminal bodies exceeded the worker snapshot budget.
func TestOnlyTheNewestTerminalCommandIDsSurviveCollection(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	oldest := completePrompt(relay, "oldest-command")
	second := completePrompt(relay, "second-command")
	for n := range retainedTerminalCommands - 1 {
		commandID := fmt.Sprintf("filler-command-%d", n)
		submitRelay(relay, commandID, prompt(strings.Repeat("x", 25*1024)))
		journalTestClaim(t, relay)
		journalTestComplete(t, relay, commandID, journalTestEndTurn())
	}
	coverWithCheckpoint(relay, "ring-barrier")
	if _, found := relay.snapshot.HandledCommands["oldest-command"]; found {
		t.Fatal("the oldest terminal ID beyond the retained count is pruned")
	}
	if repeated := submitRelay(relay, "second-command", prompt("second-command")); repeated != second {
		t.Fatalf("the newest retained IDs still answer a retry with the original acceptance: %d != %d", repeated, second)
	}
	statePath := filepath.Join(temp, core.RelayStateFile)
	if size := journalTestFileSize(t, statePath); size >= 1024*1024 {
		t.Fatalf("512 retained receipts must stay far below the snapshot budget: %d bytes", size)
	}

	relay = journalTestOpen(t, temp, session, "1.0.0")
	retainedID := fmt.Sprintf("filler-command-%d", retainedTerminalCommands-2)
	accepted := relay.snapshot.HandledCommands[retainedID].AcceptedOrdinal
	before := relay.LatestOrdinal()
	if repeated := submitRelay(relay, retainedID, prompt(strings.Repeat("x", 25*1024))); repeated != accepted {
		t.Fatalf("retained retry accepted at %d, want %d", repeated, accepted)
	}
	if relay.LatestOrdinal() != before {
		t.Fatal("a retained retry must append no work")
	}
	if claimed := journalTestClaim(t, relay); len(claimed) != 0 {
		t.Fatalf("claimed %+v", claimed)
	}
	if _, protocolError, err := relay.submitCommand(retainedID, prompt("different body")); err != nil || protocolError == nil {
		t.Fatalf("a different body under a retained id must be refused: %v %v", protocolError, err)
	}
	if renewed := submitRelay(relay, "oldest-command", prompt("oldest-command")); renewed <= oldest {
		t.Fatalf("a pruned ID is a new command again: %d <= %d", renewed, oldest)
	}
}

func TestRestartRedispatchesAPromotedConfigurationChange(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	submitRelay(relay, "config-command", setConfig("model", "sonnet"))
	// The change was started but never handed to ACP.
	if queued := queuedCommandIDs(relay); len(queued) != 0 {
		t.Fatalf("queued = %v", queued)
	}

	relay = journalTestOpen(t, temp, session, "1.0.0")
	claimed := journalTestClaim(t, relay)
	if len(claimed) != 1 || claimed[0].CommandID != "config-command" {
		t.Fatalf("claimed = %+v", claimed)
	}
	if _, setsConfig := claimed[0].Command.(core.RelayCommandSetConfig); !setsConfig {
		t.Fatalf("claimed command = %#v", claimed[0].Command)
	}
}

func TestRestartAdoptsAConfigAcceptedOutsideTheDurableQueue(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	// The prompt is started but never handed to ACP, so restarting keeps it
	// active instead of interrupting it.
	submitRelay(relay, "active-prompt", prompt("running"))
	submitRelay(relay, "config-queued", setConfig("model", "sonnet"))

	// Rewrite the durable snapshot the way an older relay wrote it: the
	// configuration change is accepted, but not in the command queue.
	statePath := filepath.Join(temp, core.RelayStateFile)
	var state jsonvalue.Object
	if err := json.Unmarshal(journalTestReadFile(t, statePath), &state); err != nil {
		t.Fatal(err)
	}
	queuedValue, _ := state.Get("queued_prompts")
	var kept []any
	for _, queued := range queuedValue.([]any) {
		if id, _ := queued.(jsonvalue.Object).Get("command_id"); id != "config-queued" {
			kept = append(kept, queued)
		}
	}
	if kept == nil {
		kept = []any{}
	}
	state.Set("queued_prompts", kept)
	journalTestWriteFile(t, statePath, []byte(journalTestJSON(t, state)))

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if queued := queuedCommandIDs(relay); !slices.Equal(queued, []string{"config-queued"}) {
		t.Fatalf("queued = %v", queued)
	}
	if claimed := journalTestClaim(t, relay); claimed[0].CommandID != "active-prompt" {
		t.Fatalf("claimed %s", claimed[0].CommandID)
	}
	finishPrompt(relay, "active-prompt")
	if claimed := journalTestClaim(t, relay); claimed[0].CommandID != "config-queued" {
		t.Fatalf("claimed %s", claimed[0].CommandID)
	}
}

func TestAcknowledgementGarbageCollectsWithoutACheckpoint(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestWarning(t, relay, "remember me")
	attach := attachRelay(relay, "attach-1", 0)
	ok, _ := attach.Body.(core.RelayResponseBodyOk)
	if attached, isAttached := ok.Payload.(core.RelayResponsePayloadAttached); !isAttached ||
		attached.ThroughOrdinal != 1 || len(attached.Events) != 1 {
		t.Fatalf("attach = %#v", attach.Body)
	}
	acknowledged := acknowledgeRelay(relay, "ack-1", 1)
	if payload, isAcknowledged := journalTestAcknowledged(acknowledged.Body); !isAcknowledged || payload.ThroughOrdinal != 1 {
		t.Fatalf("acknowledge = %#v", acknowledged.Body)
	}
	if retained := retainedEvents(relay); len(retained) != 0 {
		t.Fatalf("ACK is the durable journal retention boundary: %d events retained", len(retained))
	}

	ready := readyCheckpoint(relay, "gc-barrier")
	acknowledgeRelay(relay, "ack-checkpoint", ready.Ordinal)
	submitRelay(relay, "complete-checkpoint", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "gc-barrier"})
	for _, event := range retainedEvents(relay) {
		if event.Ordinal <= ready.Ordinal {
			t.Fatalf("event %d below the checkpoint %d is retained", event.Ordinal, ready.Ordinal)
		}
	}

	relay = journalTestOpen(t, temp, session, "1.0.0")
	stale := attachRelay(relay, "attach-stale", 0)
	failed, isError := journalTestProtocolError(stale.Body)
	detail, desynchronized := failed.Detail.(core.RelayErrorDetailDesynchronized)
	if !isError || failed.Code != core.RelayErrorCodeDesynchronized || !desynchronized ||
		detail.EarliestAvailable != ready.Ordinal {
		t.Fatalf("stale attach = %#v", stale.Body)
	}
}

func TestRelayRecoversAnEventFsyncedBeforeItsSnapshot(t *testing.T) {
	temp := t.TempDir()
	journalTestOpen(t, temp, session, "1.0.0")
	event := journalTestSignedEvent(t, core.RelayEvent{
		Format:         core.RelayEventFormatV1,
		Ordinal:        1,
		PreviousDigest: core.RelayEventGenesisDigest,
		RecordedAtMs:   core.EpochMillis(),
		Observation:    core.RelayObservationWarning{Message: "after journal fsync"},
	})
	_, path := journalTestPath(temp)
	file, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	journalTestWriteEvent(t, file, event)
	if err := file.Sync(); err != nil {
		t.Fatal(err)
	}
	file.Close()

	relay := journalTestOpen(t, temp, session, "1.0.0")
	if relay.LatestOrdinal() != 1 {
		t.Fatalf("latest ordinal %d", relay.LatestOrdinal())
	}
	events := journalTestEventsAfter(t, relay, 0, core.RelayEventGenesisDigest)
	if journalTestJSON(t, events) != journalTestJSON(t, []core.RelayEvent{event}) {
		t.Fatalf("events = %+v", events)
	}
}

// Hard-won: #1608: retrying after a short write joined the next event to the torn record.
func TestFailedJournalAppendRestoresTheBoundaryBeforeRetry(t *testing.T) {
	for _, fault := range []journalAppendFault{
		{kind: journalAppendFaultPartialWrite},
		{kind: journalAppendFaultDirectorySync},
	} {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "1.0.0")
		active := filepath.Join(temp, core.RelayJournalDir, core.RelayActiveSegment)
		observation := core.RelayObservationWarning{Message: "retryable observation"}
		var before []byte
		var operation string
		var failures int
		switch fault.kind {
		case journalAppendFaultPartialWrite:
			rtMust(relay.RecordObservation(observation))
			before = rtMust(os.ReadFile(active))
			operation, failures = "write record", 1
		case journalAppendFaultDirectorySync:
			// Check that an empty file left by the first rollback still
			// needs a directory sync on the second attempt.
			before, operation, failures = []byte{}, "sync journal directory", 2
		}
		frontier := relay.LatestOrdinal()
		digest := relay.LatestDigest()

		for range failures {
			injected := fault
			relay.journalAppendFault = &injected
			_, err := relay.RecordObservation(observation)
			if !errors.Is(err, syscall.ENOSPC) {
				t.Fatalf("error %v is not a full disk", err)
			}
			if want := fmt.Sprintf("append relay event %d to %s: %s", frontier+1, active, operation); !strings.Contains(err.Error(), want) {
				t.Fatalf("error %q does not contain %q", err, want)
			}
			after, readErr := os.ReadFile(active)
			if readErr != nil && !errors.Is(readErr, fs.ErrNotExist) {
				t.Fatal(readErr)
			}
			if !bytes.Equal(after, before) {
				t.Fatalf("active segment %q, want %q", after, before)
			}
			rtEqual(t, relay.LatestOrdinal(), frontier)
			rtEqual(t, relay.LatestDigest(), digest)
		}

		rtEqual(t, rtMust(relay.RecordObservation(observation)), frontier+1)
		rtEqual(t, rtMust(relay.RecordObservation(core.RelayObservationWarning{Message: "after retry"})), frontier+2)
		lenAfterAppend := rtMust(os.Stat(active)).Size()

		relay = journalTestOpen(t, temp, session, "1.0.0")
		attach := attachRelay(relay, "attach-after-append-failure", 0)
		ok, isOk := attach.Body.(core.RelayResponseBodyOk)
		attached, isAttached := ok.Payload.(core.RelayResponsePayloadAttached)
		if !isOk || !isAttached {
			t.Fatalf("expected attach to succeed, got %#v", attach.Body)
		}
		ordinals := []uint64{}
		for _, event := range attached.Events {
			ordinals = append(ordinals, event.Ordinal)
		}
		want := []uint64{}
		for ordinal := uint64(1); ordinal <= frontier+2; ordinal++ {
			want = append(want, ordinal)
		}
		rtEqual(t, ordinals, want)
		rtEqual(t, rtMust(os.Stat(active)).Size(), lenAfterAppend)
	}
}

// Hard-won: #1608: an unknown journal boundary after failed rollback must fence the writer.
func TestFailedJournalAppendRollbackFencesLaterAppends(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	observation := core.RelayObservationWarning{Message: "journal observation"}
	rtMust(relay.RecordObservation(observation))
	active := filepath.Join(temp, core.RelayJournalDir, core.RelayActiveSegment)
	before := rtMust(os.ReadFile(active))
	frontier := relay.LatestOrdinal()
	relay.journalAppendFault = &journalAppendFault{kind: journalAppendFaultPartialWrite, failRollback: true}
	_, err := relay.RecordObservation(observation)
	if !errors.Is(err, syscall.ENOSPC) {
		t.Fatalf("error %v is not a full disk", err)
	}
	failure := err.Error()
	if !strings.Contains(failure, "rollback failed") || !strings.Contains(failure, "truncate active segment: permission denied") {
		t.Fatalf("failure = %q", failure)
	}
	torn := rtMust(os.ReadFile(active))
	if !bytes.HasPrefix(torn, before) || len(torn) <= len(before) {
		t.Fatalf("torn segment %q does not extend %q", torn, before)
	}
	if bytes.HasSuffix(torn, []byte("\n")) {
		t.Fatal("the torn record ends with a newline")
	}

	_, err = relay.RecordObservation(observation)
	if err == nil || !strings.Contains(err.Error(), "appends disabled") || !strings.Contains(err.Error(), failure) {
		t.Fatalf("later append error = %v", err)
	}
	rtEqual(t, rtMust(os.ReadFile(active)), torn)
	rtEqual(t, relay.LatestOrdinal(), frontier)
}

func TestRelayTruncatesATornActiveTailBeforeAppendingAgain(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestWarning(t, relay, "durable prefix")
	_, active := journalTestPath(temp)
	durableLen := journalTestFileSize(t, active)

	file, err := os.OpenFile(active, os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.WriteString(`{"ordinal":2,"previous_digest":`); err != nil {
		t.Fatal(err)
	}
	file.Sync()
	file.Close()
	if journalTestFileSize(t, active) <= durableLen {
		t.Fatal("the torn tail was not written")
	}

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if relay.LatestOrdinal() != 1 {
		t.Fatalf("latest ordinal %d", relay.LatestOrdinal())
	}
	if size := journalTestFileSize(t, active); size != durableLen {
		t.Fatalf("active segment is %d bytes, want %d", size, durableLen)
	}
	journalTestWarning(t, relay, "after repair")

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if relay.LatestOrdinal() != 2 || len(retainedEvents(relay)) != 2 {
		t.Fatalf("latest ordinal %d, %d retained", relay.LatestOrdinal(), len(retainedEvents(relay)))
	}
}

// Hard-won: 97bd9199: a torn writer append previously aborted read-only attach replay.
func TestAReadOnlyReplayToleratesATornActiveTailAndServesTheNewestRecord(t *testing.T) {
	// An attach serving the hot segment can read it while the worker is
	// mid-append, catching a final record with no trailing newline. That
	// partial write must not fail the replay: every complete record —
	// including the most recent — has to be delivered, and the live file must
	// be left untouched for the writer to finish.
	temp := t.TempDir()
	path := filepath.Join(temp, core.RelayActiveSegment)
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	previousDigest := core.RelayEventGenesisDigest
	for ordinal := uint64(1); ordinal <= 3; ordinal++ {
		event := journalTestSignedEvent(t, core.RelayEvent{
			Format:         core.RelayEventFormatV1,
			Ordinal:        ordinal,
			PreviousDigest: previousDigest,
			RecordedAtMs:   core.EpochMillis(),
			Observation:    core.RelayObservationWarning{Message: fmt.Sprintf("event %d", ordinal)},
		})
		previousDigest = event.Digest
		journalTestWriteEvent(t, file, event)
	}
	// A torn, in-flight fourth record: content began but the closing bytes
	// and newline are not on disk yet.
	if _, err := file.WriteString(`{"ordinal":4,"previous_digest":`); err != nil {
		t.Fatal(err)
	}
	file.Sync()
	file.Close()
	lenBefore := journalTestFileSize(t, path)

	var seen []uint64
	if _, err := journalTestVisit(path, JournalReadModeStrict, &seen); err != nil {
		t.Fatalf("a read-only replay must tolerate a torn tail: %v", err)
	}
	if !slices.Equal(seen, []uint64{1, 2, 3}) {
		t.Fatalf("every complete record, including the most recent, must be served: %v", seen)
	}
	if size := journalTestFileSize(t, path); size != lenBefore {
		t.Fatalf("a read-only replay must not truncate the live segment: %d != %d", size, lenBefore)
	}
}

func TestRecoverModeSkipsACorruptRecordAndRecoversItsNeighbours(t *testing.T) {
	temp := t.TempDir()
	path := filepath.Join(temp, core.RelayActiveSegment)
	writeEvent := func(file *os.File, ordinal uint64, previousDigest *string) {
		event := journalTestSignedEvent(t, core.RelayEvent{
			Format:         core.RelayEventFormatV1,
			Ordinal:        ordinal,
			PreviousDigest: *previousDigest,
			RecordedAtMs:   int64(ordinal),
			Observation:    core.RelayObservationWarning{Message: fmt.Sprintf("event %d", ordinal)},
		})
		*previousDigest = event.Digest
		journalTestWriteEvent(t, file, event)
	}

	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	previousDigest := core.RelayEventGenesisDigest
	writeEvent(file, 1, &previousDigest)
	info, err := file.Stat()
	if err != nil {
		t.Fatal(err)
	}
	corruptOffset := info.Size()
	// A corrupt interior record: terminated (has its newline) but not valid
	// JSON, so its bytes are unrecoverable.
	file.WriteString(`{"ordinal":2,"observation": BROKEN` + "\n")
	writeEvent(file, 3, &previousDigest)
	file.Sync()
	file.Close()

	// Strict aborts on the corrupt record.
	var ignored []uint64
	_, err = journalTestVisit(path, JournalReadModeStrict, &ignored)
	if err == nil {
		t.Fatal("strict reads must not pass a corrupt record")
	}
	if want := fmt.Sprintf("parse relay journal %s at byte offset %d", path, corruptOffset); !strings.Contains(err.Error(), want) {
		t.Fatalf("error %q does not contain %q", err, want)
	}

	// Recover skips it and delivers both intact neighbours, reporting one gap.
	var seen []uint64
	gaps, err := journalTestVisit(path, JournalReadModeRecover, &seen)
	if err != nil {
		t.Fatalf("recovery must not fail on a corrupt record: %v", err)
	}
	if !slices.Equal(seen, []uint64{1, 3}) {
		t.Fatalf("records either side of the corruption must be recovered: %v", seen)
	}
	if len(gaps) != 1 {
		t.Fatalf("the one corrupt record is reported as a gap: %v", gaps)
	}
}

func TestRecoveryIsolatesACorruptRecordAtEveryPosition(t *testing.T) {
	// "Recover everything that isn't actively corrupted": whichever single
	// record is corrupt, all the others come back and exactly one gap is
	// reported.
	const count = 6
	for corruptIndex := uint64(1); corruptIndex <= count; corruptIndex++ {
		temp := t.TempDir()
		path := filepath.Join(temp, core.RelayActiveSegment)
		file, err := os.Create(path)
		if err != nil {
			t.Fatal(err)
		}
		for ordinal := uint64(1); ordinal <= count; ordinal++ {
			if ordinal == corruptIndex {
				file.WriteString(`{"ordinal":0,"observation": BROKEN` + "\n")
				continue
			}
			// v2 records self-hash; they carry no running frontier.
			event := journalTestSignedEvent(t, core.RelayEvent{
				Format:       core.RelayEventFormatV2,
				Ordinal:      ordinal,
				RecordedAtMs: int64(ordinal),
				Observation:  core.RelayObservationWarning{Message: fmt.Sprintf("event %d", ordinal)},
			})
			journalTestWriteEvent(t, file, event)
		}
		file.Sync()
		file.Close()

		var seen []uint64
		gaps, err := visitRelayJournalFile(path, JournalReadModeRecover, func(event core.RelayEvent, _ int) (bool, error) {
			if err := core.ValidateRelayEventSelf(&event); err != nil {
				t.Fatalf("recovered record %d fails self-validation: %v", event.Ordinal, err)
			}
			seen = append(seen, event.Ordinal)
			return false, nil
		})
		if err != nil {
			t.Fatalf("recovery must not fail on a corrupt record: %v", err)
		}
		var expected []uint64
		for ordinal := uint64(1); ordinal <= count; ordinal++ {
			if ordinal != corruptIndex {
				expected = append(expected, ordinal)
			}
		}
		if !slices.Equal(seen, expected) {
			t.Fatalf("every intact record must survive corruption at index %d: %v", corruptIndex, seen)
		}
		if len(gaps) != 1 {
			t.Fatalf("one gap for corruption at index %d: %v", corruptIndex, gaps)
		}
	}
}

func TestFailedGCPersistenceKeepsCommandIdempotencyInMemory(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	accepted := completePrompt(relay, "keep-idempotent")
	// Enough newer terminal commands that this one is due for pruning.
	for n := range retainedTerminalCommands {
		completePrompt(relay, fmt.Sprintf("newer-command-%d", n))
	}
	terminal := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	relay.snapshot.AcknowledgedThrough = terminal
	relay.snapshot.AcknowledgedDigest = digest
	relay.snapshot.RecoveryFloorOrdinal = terminal
	relay.snapshot.RecoveryFloorDigest = digest
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}

	journalTestBreakState(t, temp)
	err := relay.garbageCollectRelayHistory()
	if err == nil || !strings.Contains(err.Error(), "relay-state.json") {
		t.Fatalf("collection error = %v", err)
	}
	if _, found := relay.snapshot.HandledCommands["keep-idempotent"]; !found {
		t.Fatal("a failed write forgot the command ID")
	}
	if repeated := submitRelay(relay, "keep-idempotent", prompt("keep-idempotent")); repeated != accepted {
		t.Fatalf("retry accepted at %d, want %d", repeated, accepted)
	}
}

func TestRetryResumesARelayLocalCommandAfterSnapshotPersistenceFailure(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	statePath := journalTestBreakState(t, temp)
	command := core.RelayCommandClearQueuedPrompts{}

	failed := relay.Handle(relayRequest("first-local-attempt", core.RelayRequestSubmit{
		CommandID: "retry-local",
		Command:   command,
	}))
	if !journalTestErrorCode(failed.Body, core.RelayErrorCodeInternal) {
		t.Fatalf("first attempt = %#v", failed.Body)
	}
	if state := relay.snapshot.Dispatches["retry-local"].State; state != core.RelayDispatchStateQueued {
		t.Fatalf("dispatch state = %v", state)
	}
	if !slices.ContainsFunc(retainedEvents(relay), func(event core.RelayEvent) bool {
		_, queued := event.Observation.(core.RelayObservationCommandQueued)
		return queued && event.CommandID != nil && *event.CommandID == "retry-local"
	}) {
		t.Fatal("the failed attempt did not journal its acceptance")
	}

	if err := os.Remove(statePath); err != nil {
		t.Fatal(err)
	}
	retried := relay.Handle(relayRequest("retry-local-attempt", core.RelayRequestSubmit{
		CommandID: "retry-local",
		Command:   command,
	}))
	ok, _ := retried.Body.(core.RelayResponseBodyOk)
	if _, accepted := ok.Payload.(core.RelayResponsePayloadAccepted); !accepted {
		t.Fatalf("retry = %#v", retried.Body)
	}
	if _, found := relay.snapshot.Dispatches["retry-local"]; found {
		t.Fatal("the finished local command still has a dispatch")
	}
	if relay.snapshot.HandledCommands["retry-local"].TerminalOrdinal == nil {
		t.Fatal("the local command has no terminal ordinal")
	}
}

func TestDuplicateAcknowledgementRetriesIncompleteJournalGC(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestWarning(t, relay, "collect after retry")
	through := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	relay.snapshot.AcknowledgedThrough = through
	relay.snapshot.AcknowledgedDigest = digest
	relay.snapshot.RecoveryFloorOrdinal = through
	relay.snapshot.RecoveryFloorDigest = digest
	// The snapshot stays unpersisted, so the collection below has to make the
	// retained frontier durable before it drops history under it.

	statePath := journalTestBreakState(t, temp)
	failed := relay.Handle(relayRequest("gc-fails-after-ack", core.RelayRequestAcknowledge{
		ThroughOrdinal: through,
		ThroughDigest:  digest,
	}))
	if !journalTestErrorCode(failed.Body, core.RelayErrorCodeInternal) {
		t.Fatalf("first acknowledgement = %#v", failed.Body)
	}

	if err := os.Remove(statePath); err != nil {
		t.Fatal(err)
	}
	retried := relay.Handle(relayRequest("gc-retry-after-ack", core.RelayRequestAcknowledge{
		ThroughOrdinal: through,
		ThroughDigest:  digest,
	}))
	if acknowledged, ok := journalTestAcknowledged(retried.Body); !ok || acknowledged.ThroughOrdinal != through {
		t.Fatalf("retried acknowledgement = %#v", retried.Body)
	}
	if count := relay.retainedEventBodyCount(); count != 0 {
		t.Fatalf("%d hot events retained", count)
	}
	if events := journalTestEventsAfter(t, relay, through, digest); len(events) != 0 {
		t.Fatalf("events after the acknowledgement: %+v", events)
	}
}

func TestFailedAckPersistenceDoesNotAdvanceTheLiveCursor(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestWarning(t, relay, "retain until ACK is durable")
	digest := relay.LatestDigest()
	statePath := journalTestBreakState(t, temp)

	failed := relay.Handle(relayRequest("ack-persistence-fails", core.RelayRequestAcknowledge{
		ThroughOrdinal: 1,
		ThroughDigest:  digest,
	}))
	if !journalTestErrorCode(failed.Body, core.RelayErrorCodeInternal) {
		t.Fatalf("acknowledgement = %#v", failed.Body)
	}
	if relay.AcknowledgedThrough() != 0 || len(retainedEvents(relay)) != 1 {
		t.Fatalf("acknowledged through %d, %d retained", relay.AcknowledgedThrough(), len(retainedEvents(relay)))
	}

	if err := os.Remove(statePath); err != nil {
		t.Fatal(err)
	}
	retry := relay.Handle(relayRequest("ack-persistence-retry", core.RelayRequestAcknowledge{
		ThroughOrdinal: 1,
		ThroughDigest:  digest,
	}))
	if acknowledged, ok := journalTestAcknowledged(retry.Body); !ok || acknowledged.ThroughOrdinal != 1 {
		t.Fatalf("retry = %#v", retry.Body)
	}
}

func TestFailedClaimPersistenceLeavesTheCommandClaimable(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	submitRelay(relay, "claim-retry", prompt("run once"))
	if state := relay.snapshot.Dispatches["claim-retry"].State; state != core.RelayDispatchStatePending {
		t.Fatalf("dispatch state = %v", state)
	}
	statePath := journalTestBreakState(t, temp)

	if _, err := relay.ClaimPendingCommands(true); err == nil {
		t.Fatal("claim succeeded without a durable snapshot")
	}
	if state := relay.snapshot.Dispatches["claim-retry"].State; state != core.RelayDispatchStatePending {
		t.Fatalf("dispatch state after the failed claim = %v", state)
	}

	if err := os.Remove(statePath); err != nil {
		t.Fatal(err)
	}
	claimed := journalTestClaim(t, relay)
	if len(claimed) != 1 || claimed[0].CommandID != "claim-retry" {
		t.Fatalf("claimed = %+v", claimed)
	}
	if state := relay.snapshot.Dispatches["claim-retry"].State; state != core.RelayDispatchStateInFlight {
		t.Fatalf("dispatch state after the claim = %v", state)
	}
}

// A relay that truncated an event must still be able to reopen the journal
// it wrote — the readback path bounds lines by the same budget.
// Hard-won: be7c009d: an oversized ACP observation previously made append fatal to the live worker.
func TestATruncatedEventCanBeReadBackAfterReopening(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestSessionUpdate(t, relay, journalTestTextChunk(strings.Repeat("y", 3*1024*1024)))

	if reopened := journalTestOpen(t, temp, session, "1.0.0"); reopened.LatestOrdinal() != 1 {
		t.Fatalf("latest ordinal %d", reopened.LatestOrdinal())
	}
}

func TestSealedRelaySegmentsReplayAndAreRemovedAfterAcknowledgement(t *testing.T) {
	temp := t.TempDir()
	journal, _ := journalTestPath(temp)
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestWarning(t, relay, "sealed")
	index := slices.IndexFunc(relay.journalSpans, func(span RelayJournalSpan) bool {
		return filepath.Base(span.path) == core.RelayActiveSegment
	})
	metadata := relay.journalSpans[index]
	if err := sealActiveRelaySegment(journal, &metadata); err != nil {
		t.Fatal(err)
	}
	if len(journalTestGzFiles(t, journal)) == 0 {
		t.Fatal("no sealed segment was written")
	}

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if events := journalTestEventsAfter(t, relay, 0, core.RelayEventGenesisDigest); len(events) != 1 {
		t.Fatalf("%d events replayed", len(events))
	}
	attachRelay(relay, "attach-sealed", 0)
	acknowledgeRelay(relay, "ack-sealed", 1)
	if sealed := journalTestGzFiles(t, journal); len(sealed) != 0 {
		t.Fatalf("sealed segments survived acknowledgement: %v", sealed)
	}
}

func TestReopeningPreservesAStaleActiveCopyBeforeAppending(t *testing.T) {
	temp := t.TempDir()
	journal, active := journalTestPath(temp)
	relay := journalTestOpen(t, temp, session, "1.0.0")
	for _, message := range []string{"first", "second"} {
		journalTestWarning(t, relay, message)
	}
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}

	activeBytes := journalTestReadFile(t, active)
	firstLineEnd := bytes.IndexByte(activeBytes, '\n') + 1
	staleBytes := slices.Clone(activeBytes[:firstLineEnd])
	metadata, err := inspectRelayJournalFile(active, false)
	if err != nil || metadata == nil {
		t.Fatalf("inspect active: %v", err)
	}
	if err := sealActiveRelaySegment(journal, metadata); err != nil {
		t.Fatal(err)
	}
	journalTestWriteFile(t, active, staleBytes)

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if relay.LatestOrdinal() != 2 {
		t.Fatalf("latest ordinal %d", relay.LatestOrdinal())
	}
	if len(journalTestReadFile(t, active)) != 0 {
		t.Fatal("the stale active segment was not replaced")
	}
	entries, err := os.ReadDir(journal)
	if err != nil {
		t.Fatal(err)
	}
	archived := ""
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), "stale-active-") {
			archived = filepath.Join(journal, entry.Name())
			break
		}
	}
	if archived == "" {
		t.Fatal("stale active journal was not preserved")
	}
	if !bytes.Equal(journalTestReadFile(t, archived), staleBytes) {
		t.Fatal("the preserved copy differs from the stale active segment")
	}

	journalTestWarning(t, relay, "third")
	if relay.LatestOrdinal() != 3 {
		t.Fatalf("latest ordinal %d", relay.LatestOrdinal())
	}
}

func TestReplayLazilyResolvesAnOldSealedSegmentBoundary(t *testing.T) {
	temp := t.TempDir()
	journal, active := journalTestPath(temp)
	relay := journalTestOpen(t, temp, session, "1.0.0")
	firstDigest := ""
	for index := 0; index <= core.RelayHotEventCapacity; index++ {
		journalTestWarning(t, relay, fmt.Sprintf("event %d", index))
		if index == 0 {
			firstDigest = relay.LatestDigest()
		}
		span := slices.IndexFunc(relay.journalSpans, func(span RelayJournalSpan) bool { return span.path == active })
		if err := sealActiveRelaySegment(journal, &relay.journalSpans[span]); err != nil {
			t.Fatal(err)
		}
	}
	if err := relay.persistSnapshot(); err != nil {
		t.Fatal(err)
	}

	relay = journalTestOpen(t, temp, session, "1.0.0")
	events, err := relay.EventsAfter(1, firstDigest)
	if err != nil {
		t.Fatalf("old segment boundary should resolve from the next sealed segment: %v", err)
	}
	if events[0].Ordinal != 2 {
		t.Fatalf("first event %d", events[0].Ordinal)
	}
}

func TestLargeRelayHistoryStaysDiskBackedAndReplaysAcrossSegments(t *testing.T) {
	const eventCount = 80
	const messageBytes = 64 * 1024

	temp := t.TempDir()
	journal, _ := journalTestPath(temp)
	relay := journalTestOpen(t, temp, session, "1.0.0")
	for index := range eventCount {
		journalTestWarning(t, relay, fmt.Sprintf("%04d:%s", index, strings.Repeat("x", messageBytes)))
	}
	if count := relay.retainedEventBodyCount(); count != core.RelayHotEventCapacity {
		t.Fatalf("%d hot events", count)
	}
	if sealed := journalTestGzFiles(t, journal); len(sealed) < 2 {
		t.Fatal("test history did not cross multiple sealed journal files")
	}
	expectedLatest := relay.LatestOrdinal()
	expectedDigest := relay.LatestDigest()

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if relay.LatestOrdinal() != expectedLatest || relay.LatestDigest() != expectedDigest {
		t.Fatalf("reopened at %d", relay.LatestOrdinal())
	}
	if count := relay.retainedEventBodyCount(); count != core.RelayHotEventCapacity {
		t.Fatalf("%d hot events after reopening", count)
	}

	cursor := core.RelayCursor{Ordinal: 0, Digest: core.RelayEventGenesisDigest}
	replayed, pages := 0, 0
	for cursor.Ordinal < expectedLatest {
		response := relay.Handle(relayRequest(fmt.Sprintf("paged-replay-%d", pages), core.RelayRequestAttach{
			AfterOrdinal: cursor.Ordinal,
			AfterDigest:  cursor.Digest,
		}))
		ok, _ := response.Body.(core.RelayResponseBodyOk)
		attached, isAttached := ok.Payload.(core.RelayResponsePayloadAttached)
		if !isAttached {
			t.Fatalf("disk-backed replay failed: %#v", response.Body)
		}
		if len(attached.Events) == 0 {
			t.Fatal("an attach page was empty")
		}
		for _, event := range attached.Events {
			if err := core.ValidateRelayEvent(cursor.Ordinal, cursor.Digest, &event); err != nil {
				t.Fatal(err)
			}
			cursor.Ordinal = event.Ordinal
			cursor.Digest = event.Digest
		}
		if cursor.Ordinal != attached.ThroughOrdinal || cursor.Digest != attached.ThroughDigest {
			t.Fatalf("page ended at %d, through %d", cursor.Ordinal, attached.ThroughOrdinal)
		}
		replayed += len(attached.Events)
		pages++
	}
	if replayed != eventCount {
		t.Fatalf("replayed %d events", replayed)
	}
	if pages < 2 {
		t.Fatal("history unexpectedly fit in one replay page")
	}
	if cursor.Digest != expectedDigest {
		t.Fatal("replay ended on the wrong digest")
	}
}

func TestReopeningDoesNotDecompressHistoricalSealedSegments(t *testing.T) {
	const eventCount = 80
	const messageBytes = 64 * 1024

	temp := t.TempDir()
	journal, _ := journalTestPath(temp)
	relay := journalTestOpen(t, temp, session, "1.0.0")
	for index := range eventCount {
		journalTestWarning(t, relay, fmt.Sprintf("%04d:%s", index, strings.Repeat("x", messageBytes)))
	}

	sealed := journalTestGzFiles(t, journal)
	if len(sealed) < 3 {
		t.Fatal("test history did not seal enough segments")
	}
	journalTestWriteFile(t, sealed[0], []byte("not a gzip stream"))

	relay, err := DurableRelayOpen(temp, session, "1.0.0")
	if err != nil {
		t.Fatalf("current snapshot should open without reading old transcript segments: %v", err)
	}
	response := relay.Handle(relayRequest("attach-corrupt-history", core.RelayRequestAttach{
		AfterOrdinal: 0,
		AfterDigest:  core.RelayEventGenesisDigest,
	}))
	failed, isError := journalTestProtocolError(response.Body)
	if !isError {
		t.Fatalf("corrupt history was served: %#v", response)
	}
	// Corrupt history is never served as valid data. Because readable history
	// exists past the corrupt segment, the relay answers with a recovery
	// cursor so the controller resynchronizes forward instead of retrying the
	// unreadable bytes forever.
	if failed.Code != core.RelayErrorCodeDesynchronized {
		t.Fatalf("error code %v", failed.Code)
	}
	detail, ok := failed.Detail.(core.RelayErrorDetailDesynchronized)
	if !ok {
		t.Fatalf("expected a desync recovery cursor: %#v", failed)
	}
	if detail.EarliestAvailable == 0 {
		t.Fatalf("recovery cursor must skip the corrupt segment, got %d", detail.EarliestAvailable)
	}
	if detail.EarliestAvailable >= detail.Latest {
		t.Fatalf("newer readable history must remain available: %d < %d", detail.EarliestAvailable, detail.Latest)
	}
}

const journalTestFrontierDigest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

// Hard-won: ce467bc6f: absent restored assessment history erased evidence replayed from the journal.
func TestRestoredRelayContinuesAfterCanonicalEventFrontier(t *testing.T) {
	temp := t.TempDir()
	journalTestWriteFile(t, core.RestoredRelaySeedPath(temp), []byte(`{"event_frontier":41,`+
		`"event_frontier_digest":"`+journalTestFrontierDigest+`",`+
		`"queued_prompts":[{"command_id":"restored-command","content":[{"type":"text","text":"continue offline"}],"queued_at_ms":1234}]}`))

	relay := journalTestOpen(t, temp, session, "1.0.0")
	if relay.LatestOrdinal() != 42 || relay.AcknowledgedThrough() != 41 {
		t.Fatalf("latest %d, acknowledged %d", relay.LatestOrdinal(), relay.AcknowledgedThrough())
	}
	if claimed := journalTestClaim(t, relay); claimed[0].CommandID != "restored-command" {
		t.Fatalf("claimed %s", claimed[0].CommandID)
	}
	if ordinal := journalTestWarning(t, relay, "restored"); ordinal != 43 {
		t.Fatalf("warning recorded at %d", ordinal)
	}

	relay = journalTestOpen(t, temp, session, "1.0.0")
	if events := journalTestEventsAfter(t, relay, 41, journalTestFrontierDigest); events[0].Ordinal != 42 {
		t.Fatalf("first event %d", events[0].Ordinal)
	}
	// Hard-won: 7211dfd7b: an unavailable saved prompt became an initialized empty tail.
	relay = journalTestOpen(t, temp, session, "recovery-still-queued")
	recovery := relay.snapshot.QueuedPrompts[0].CommandID
	journalTestSessionConfigured(t, relay)
	journalTestClaim(t, relay)
	facts := core.ActivityFacts{}
	if tail := relay.TurnContext().Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &facts, 0).UserPromptTail; tail != "continue offline" {
		t.Fatalf("user prompt tail = %q", tail)
	}

	journalTestComplete(t, relay, recovery, journalTestEndTurn())
	submitRelay(relay, "post-restore-prompt", prompt("new work after restoration"))
	journalTestClaim(t, relay)
	journalTestSessionUpdate(t, relay, journalTestTextChunk("actual restored-session reply"))
	if relay.snapshot.AssessmentContext != nil {
		t.Fatal("the restored relay has assessment context")
	}
	relay = journalTestOpen(t, temp, session, "reopened-without-assessment")
	evidence := relay.TurnContext().Evidence(core.HarnessKindCodex, core.TurnPhaseRunning, &facts, 0)
	if evidence.UserPromptTail != "new work after restoration" ||
		evidence.AssistantTextTail != "actual restored-session reply" {
		t.Fatalf("evidence tails = %q / %q", evidence.UserPromptTail, evidence.AssistantTextTail)
	}
}

func TestRestoredRelayRebuildsAQueuedConfigurationChange(t *testing.T) {
	temp := t.TempDir()
	journalTestWriteFile(t, core.RestoredRelaySeedPath(temp), []byte(`{"event_frontier":41,`+
		`"event_frontier_digest":"`+journalTestFrontierDigest+`",`+
		`"queued_prompts":[`+
		`{"command_id":"restored-config","kind":{"set_config":{"key":"model","value":"sonnet"}},"content":[{"type":"text","text":"/model sonnet"}],"queued_at_ms":1234},`+
		`{"command_id":"restored-prompt","content":[{"type":"text","text":"continue offline"}],"queued_at_ms":1235}]}`))

	relay := journalTestOpen(t, temp, session, "1.0.0")
	claimed := journalTestClaim(t, relay)
	if len(claimed) != 1 || claimed[0].CommandID != "restored-config" {
		t.Fatalf("claimed = %+v", claimed)
	}
	if !reflect.DeepEqual(claimed[0].Command, setConfig("model", "sonnet")) {
		t.Fatalf("a restored configuration change must not become a prompt: %#v", claimed[0].Command)
	}

	journalTestComplete(t, relay, "restored-config", core.RelayCommandOutcomeConfigured{})
	if claimed := journalTestClaim(t, relay); claimed[0].CommandID != "restored-prompt" {
		t.Fatalf("claimed %s", claimed[0].CommandID)
	}
}

// Session teardown deletes the worker root while its daemon may still be
// alive. A durable write that recreated the root would leave a snapshot with
// no journal behind it, and no later resume could reopen that.
// Hard-won: 2e7f5ec9: a detached worker recreated a closed session root and broke the next resume.
func TestRelayWritesFailInsteadOfRecreatingADeletedWorkerRoot(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker-root")
	relay := journalTestOpen(t, root, session, "1.0.0")
	journalTestWarning(t, relay, "before teardown")
	through := relay.LatestOrdinal()
	digest := relay.LatestDigest()
	if err := os.RemoveAll(root); err != nil {
		t.Fatal(err)
	}

	// Acknowledging writes the snapshot and nothing else, so this is the path
	// that used to recreate the root behind teardown's back.
	acknowledged := relay.Handle(relayRequest("acknowledge-after-teardown", core.RelayRequestAcknowledge{
		ThroughOrdinal: through,
		ThroughDigest:  digest,
	}))
	if !journalTestErrorCode(acknowledged.Body, core.RelayErrorCodeInternal) {
		t.Fatalf("acknowledging into a removed root must fail: %#v", acknowledged.Body)
	}
	if relayPathExists(root) {
		t.Fatal("the snapshot write resurrected the root")
	}

	if _, err := relay.RecordObservation(core.RelayObservationWarning{Message: "after teardown"}); err == nil {
		t.Fatal("a journal append into a removed root succeeded")
	}
	if relayPathExists(root) {
		t.Fatal("the journal append resurrected the root")
	}
}

func TestRestoredRelayRejectsAnInvalidCanonicalFrontier(t *testing.T) {
	for _, seed := range []string{
		// Unparseable frontier.
		`{"event_frontier":"forty-one"}`,
		// Well-formed JSON whose digest is not a relay event digest.
		`{"event_frontier":41,"event_frontier_digest":"nope"}`,
		// A non-genesis frontier claiming the genesis digest.
		`{"event_frontier":41,"event_frontier_digest":"` + core.RelayEventGenesisDigest + `"}`,
	} {
		temp := t.TempDir()
		journalTestWriteFile(t, core.RestoredRelaySeedPath(temp), []byte(seed))
		_, err := DurableRelayOpen(temp, session, "1.0.0")
		if err == nil {
			t.Fatalf("invalid frontier should fail: %s", seed)
		}
		if !strings.Contains(err.Error(), core.RestoredRelaySeedFile) {
			t.Fatalf("%v", err)
		}
		if relayPathExists(filepath.Join(temp, core.RelayStateFile)) {
			t.Fatal("a rejected seed wrote a snapshot")
		}
	}
}

// Hard-won: 50a0277d2: terminal bodies made a worker's own snapshot impossible to reopen.
func TestOversizedLegacyTerminalLedgerOpensAndPersistsCompactly(t *testing.T) {
	temp := t.TempDir()
	snapshot := core.NewRelaySnapshot(session)
	withMeta := func(meta string) core.RelayCommand {
		command := prompt(strings.Repeat("x", 25*1024)).(core.RelayCommandPrompt)
		var object jsonvalue.Object
		if err := json.Unmarshal([]byte(meta), &object); err != nil {
			t.Fatal(err)
		}
		command.Prompt[0].Text.Meta = object
		return command
	}
	command := withMeta(`{"z":{"b":2,"a":1},"a":0}`)
	for index := uint64(0); index < 330; index++ {
		id := fmt.Sprintf("legacy-%d", index)
		terminal := index*2 + 2
		snapshot.HandledCommands[id] = core.HandledRelayCommand{
			Command:         command,
			AcceptedOrdinal: index*2 + 1,
			TerminalOrdinal: &terminal,
			Outcome:         journalTestEndTurn(),
		}
		snapshot.Dispatches[id] = core.RelayDispatchRecord{Command: command, State: core.RelayDispatchStateCompleted}
	}
	snapshot.FormatVersion = 19
	snapshot.LatestOrdinal = 660
	snapshot.LatestDigest = strings.Repeat("a", 64)
	snapshot.AcknowledgedThrough = snapshot.LatestOrdinal
	snapshot.AcknowledgedDigest = snapshot.LatestDigest
	snapshot.RecoveryFloorOrdinal = snapshot.LatestOrdinal
	snapshot.RecoveryFloorDigest = snapshot.LatestDigest
	// Serializing without the optional identity produces the actual shipped
	// shape.
	legacy, err := workerRuntimeMarshalPretty(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	if len(legacy) <= core.RelaySnapshotByteBudget {
		t.Fatalf("legacy snapshot is only %d bytes", len(legacy))
	}
	path := filepath.Join(temp, core.RelayStateFile)
	journalTestWriteFile(t, path, legacy)
	relay := journalTestOpen(t, temp, session, "1.0.0")
	if compact := journalTestReadFile(t, path); len(compact) >= 256*1024 {
		t.Fatalf("compacted snapshot is %d bytes", len(compact))
	}
	if len(relay.snapshot.Dispatches) != 0 {
		t.Fatalf("%d dispatches kept", len(relay.snapshot.Dispatches))
	}
	for id, handled := range relay.snapshot.HandledCommands {
		if handled.Command != nil {
			t.Fatalf("receipt %s kept its command body", id)
		}
	}
	reordered := withMeta(`{"a":0,"z":{"a":1,"b":2}}`)
	if !relayCommandEqual(command, reordered) {
		t.Fatal("reordered metadata is not the same command")
	}
	if accepted := submitRelay(relay, "legacy-329", reordered); accepted != 659 {
		t.Fatalf("legacy retry accepted at %d", accepted)
	}
	if relay.LatestOrdinal() != 660 {
		t.Fatalf("latest ordinal %d", relay.LatestOrdinal())
	}
	journalTestOpen(t, temp, session, "1.0.0")
}

// Hard-won: 1c150431b: a never-delivered request vanished from the recovery prompt.
// Hard-won: 00050db89: caller-owned recovery IDs could prevent every subsequent restart.
// Hard-won: ebb2e9611: a caller-held generation wedged restart recovery.
func TestRestartContinuationRecoveryIsIdempotentAndPreservesNeverDeliveredBlocks(t *testing.T) {
	originalID := strings.Repeat("a", 128)
	for cut := 0; cut <= 2; cut++ {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "1.0.0")
		originalBlocks := []acpschema.ContentBlock{
			journalTestTextBlock("finish the task"),
			{Kind: acpschema.ContentBlockKindImage, Image: &acpschema.ImageContent{Data: "aW1hZ2U=", MimeType: "image/png"}},
			journalTestTextBlock("Keep the original image and these Unicode instructions: 日本語."),
		}
		submitRelay(relay, originalID, core.RelayCommandPrompt{Prompt: core.DeepClone(originalBlocks)})
		journalTestClaim(t, relay)
		submitRelay(relay, "later-user-input", prompt("next task"))
		occupied := core.RestartContinuationID(originalID)
		occupiedNext := core.RestartContinuationID(occupied)
		for _, id := range []string{occupied, occupiedNext} {
			submitRelay(relay, id, core.RelayCommandNotificationsRead{Through: 0})
		}
		recoveryID := core.RestartContinuationID(occupiedNext)
		if cut == 1 {
			if _, err := relay.RecordCommandInterrupted(originalID, core.OutcomeReasonWorkerRestarted, "worker restarted"); err != nil {
				t.Fatal(err)
			}
		}
		if cut == 2 {
			// The one durable event exists, but cleanup/promotion did not run.
			reason := core.OutcomeReasonWorkerRestarted
			continuation := recoveryID
			journalTestAppend(t, relay, originalID, core.RelayObservationCommandInterrupted{
				ContinuationID: &continuation,
				CommandID:      originalID,
				Command:        core.RelayCommandKindPrompt,
				Reason:         &reason,
				Message:        "worker restarted",
			})
		}
		for range 2 {
			relay = journalTestOpen(t, temp, session, "1.0.0")
			before := relay.LatestOrdinal()
			if _, err := relay.RecordCommandInterrupted(originalID, core.OutcomeReasonWorkerRestarted, "repeated recovery"); err != nil {
				t.Fatal(err)
			}
			if relay.LatestOrdinal() != before {
				t.Fatalf("recovery mode %d: a repeated interruption appended an event", cut)
			}
		}
		relay = journalTestOpen(t, temp, session, "1.0.0")
		continued := 0
		for _, event := range retainedEvents(relay) {
			if interrupted, ok := event.Observation.(core.RelayObservationCommandInterrupted); ok &&
				interrupted.ContinuationID != nil && *interrupted.ContinuationID == recoveryID {
				continued++
			}
		}
		if continued != 1 {
			t.Fatalf("recovery mode %d: %d continuation events", cut, continued)
		}
		journalTestSessionConfigured(t, relay)
		claimed := journalTestClaim(t, relay)
		if len(claimed) != 1 || claimed[0].CommandID != recoveryID {
			t.Fatalf("recovery mode %d: claimed %+v", cut, claimed)
		}
		recoveryBlocks := core.DeepClone(journalTestBlocksForCommand(t, claimed[0]))
		if journalTestJSON(t, journalTestPromptBlocks(t, claimed[0].Command)) != journalTestJSON(t, originalBlocks) {
			t.Fatalf("recovery mode %d: recovery blocks lost the original request", cut)
		}
		if len(recoveryBlocks) != 1 {
			t.Fatalf("recovery must not replay the request: %d blocks", len(recoveryBlocks))
		}
		if recoveryBlocks[0].Kind != acpschema.ContentBlockKindText || recoveryBlocks[0].Text == nil {
			t.Fatal("missing recovery note")
		}
		note := recoveryBlocks[0].Text.Text
		if !core.IsControlOriginPrompt(note) {
			t.Fatalf("recovery note is not control-origin: %q", note)
		}
		if core.IsUserAuthoredMessage(&recoveryID, note) {
			t.Fatalf("recovery note reads as user-authored: %q", note)
		}
		if !strings.Contains(note, "check whether it already took effect") || !strings.Contains(note, "external side effects") {
			t.Fatalf("recovery note = %q", note)
		}

		// A second restart interrupts recovery itself: it gets its own wake,
		// still ahead of the user's queued work, without a retry budget.
		relay = journalTestOpen(t, temp, session, "1.0.0")
		journalTestSessionConfigured(t, relay)
		second := journalTestClaim(t, relay)
		if len(second) != 1 || second[0].CommandID != core.RestartContinuationID(recoveryID) {
			t.Fatalf("second recovery claimed %+v", second)
		}
		if journalTestJSON(t, journalTestBlocksForCommand(t, second[0])) != journalTestJSON(t, recoveryBlocks) {
			t.Fatal("a second restart must send only the recovery note")
		}
		journalTestComplete(t, relay, second[0].CommandID, journalTestEndTurn())
		if claimed := journalTestClaim(t, relay); claimed[0].CommandID != "later-user-input" {
			t.Fatalf("claimed %s", claimed[0].CommandID)
		}
	}
}

func TestRestartContinuationHonorsCancellationEvenWhenTheWorkerDiesBeforeAck(t *testing.T) {
	for _, cancel := range []core.RelayCommand{
		nil,
		core.RelayCommandCancelTurn{},
		core.RelayCommandCancelTurnFor{ActivePromptID: "interrupted-turn"},
	} {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "1.0.0")
		submitRelay(relay, "interrupted-turn", prompt("finish the task"))
		journalTestClaim(t, relay)
		if cancel != nil {
			submitRelay(relay, "cancel-command", cancel)
			journalTestClaim(t, relay)
		} else if _, err := relay.RecordCommandInterrupted("interrupted-turn", core.OutcomeReasonRequestedCancellation, "user pressed Esc"); err != nil {
			t.Fatal(err)
		}
		relay = journalTestOpen(t, temp, session, "1.0.0")
		if claimed := journalTestClaim(t, relay); len(claimed) != 0 {
			t.Fatalf("claimed %+v after cancellation %#v", claimed, cancel)
		}
		if journalTestHasRestartContinuation(relay) {
			t.Fatalf("a cancelled turn was continued after %#v", cancel)
		}
	}
}

// Hard-won: 00050db89: permission Cancel did not suppress recovery of the stopped prompt.
func TestPermissionCancellationPreventsRestartContinuationBeforePromptSettles(t *testing.T) {
	for _, testCase := range []struct {
		id      string
		action  string
		stopped bool
		stale   bool
	}{
		{"tool-permission-1", "cancel", true, false},
		{"tool-permission-1", "cancel", false, true},
		{"tool-permission-1", "decline", false, false},
		{"tool-permission-1", "closed", false, false},
		{"ordinary-question", "cancel", false, false},
	} {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "test")
		submitRelay(relay, "original", prompt("finish the task"))
		journalTestClaim(t, relay)
		request := core.ElicitationRequest{ID: testCase.id, Message: "Allow command?", Fields: []core.ElicitationField{}}
		if strings.HasPrefix(testCase.id, "tool-permission-") {
			request.ToolPermission = &core.ToolPermissionForm{Skip: true}
		}
		journalTestRecord(t, relay, core.RelayObservationElicitationRequested{Request: request})
		target := "original"
		if testCase.stale {
			journalTestComplete(t, relay, "original", journalTestEndTurn())
			submitRelay(relay, "newer-prompt", prompt("new task"))
			journalTestClaim(t, relay)
			target = "newer-prompt"
		}
		journalTestRecord(t, relay, core.RelayObservationElicitationResolved{ElicitationID: testCase.id, Action: testCase.action})
		cancelling := relay.snapshot.CancellingPromptID
		if testCase.stopped != (cancelling != nil && *cancelling == target) || (!testCase.stopped && cancelling != nil) {
			t.Fatalf("%+v: cancelling prompt = %v", testCase, cancelling)
		}
		// The prompt has not acknowledged cancellation. Crash now.
		relay = journalTestOpen(t, temp, session, "test")
		_, continued := relay.snapshot.HandledCommands[core.RestartContinuationID(target)]
		if continued != !testCase.stopped {
			t.Fatalf("%+v: continued = %v", testCase, continued)
		}
	}
}

// Hard-won: 1d399ac64: interrupt during reload left queued recovery running afterwards.
func TestInterruptQueuedRecoveryFinishesLocallyAcrossPartialCompletion(t *testing.T) {
	for cut := range 3 {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "test")
		submitRelay(relay, "original-turn", prompt("first task"))
		journalTestClaim(t, relay)
		submitRelay(relay, "later-prompt", prompt("second task"))
		relay = journalTestOpen(t, temp, session, "test")
		target := core.RestartContinuationID("original-turn")
		if cut == 0 {
			submitRelay(relay, "interrupt-reload", core.RelayCommandCancelTurn{})
		} else {
			journalTestAppend(t, relay, "interrupt-reload", core.RelayObservationCommandQueued{
				CommandID:   "interrupt-reload",
				Command:     core.RelayCommandCancelTurn{},
				CreatedAtMs: 1,
			})
			if cancelling := relay.snapshot.CancellingPromptID; cancelling == nil || *cancelling != target {
				t.Fatalf("cut %d: cancelling prompt = %v", cut, cancelling)
			}
			if cut == 2 {
				if _, err := relay.RecordCommandInterrupted(target, core.OutcomeReasonRequestedCancellation,
					"User cancelled queued restart recovery"); err != nil {
					t.Fatal(err)
				}
			}
		}
		relay = journalTestOpen(t, temp, session, "test")
		if _, cancelled := relay.snapshot.HandledCommands["interrupt-reload"].Outcome.(core.RelayCommandOutcomeCancelled); !cancelled {
			t.Fatalf("cut %d: interrupt outcome = %#v", cut, relay.snapshot.HandledCommands["interrupt-reload"].Outcome)
		}
		if relay.snapshot.HandledCommands[target].TerminalOrdinal == nil {
			t.Fatalf("cut %d: the recovery never finished", cut)
		}
		if relay.snapshot.CancellingPromptID != nil {
			t.Fatalf("cut %d: still cancelling %s", cut, *relay.snapshot.CancellingPromptID)
		}
		events := retainedEvents(relay)
		for _, event := range events {
			if started, ok := event.Observation.(core.RelayObservationCommandStarted); ok &&
				(started.CommandID == target || started.CommandID == "interrupt-reload") {
				t.Fatalf("cut %d: %s started", cut, started.CommandID)
			}
		}
		projection := core.MaterializedSessionEmpty(session)
		for _, event := range events {
			update, err := transcript.ProjectRelayEvent(&projection, &event)
			if err != nil {
				t.Fatal(err)
			}
			if err := transcript.ApplyCommittedProjectionEvent(&projection, &event, update.Mutation); err != nil {
				t.Fatal(err)
			}
		}
		if kind := projection.LastTurnOutcome.Result().Kind; kind != core.TurnResultKindCancelled {
			t.Fatalf("cut %d: last turn outcome %v", cut, kind)
		}
		journalTestSessionConfigured(t, relay)
		next := journalTestClaim(t, relay)
		if len(next) != 1 || next[0].CommandID != "later-prompt" {
			t.Fatalf("cut %d: claimed %+v", cut, next)
		}
	}
}

// Hard-won: d2f26b4c6: goal recovery suppressed ordinary interrupted requests.
func TestRestartContinuationPreservesOrdinaryUserWorkAlongsideANativeGoal(t *testing.T) {
	for _, nativeID := range []string{"", "native-goal-session"} {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "1.0.0")
		if nativeID != "" {
			journalTestRecord(t, relay, core.RelayObservationSessionOpened{NativeSessionID: nativeID, Resumed: true})
		}
		var update acpschema.SessionUpdate
		if err := json.Unmarshal([]byte(`{"sessionUpdate":"session_info_update","_meta":{`+
			`"mjGoalCapability":{"version":1,"controlMethod":"_session/goal","actions":["resume","pause"]},`+
			`"goal":{"objective":"finish campaign","status":"active","createdAt":123,"controlMethod":"_session/goal"},`+
			`"execution":{"version":1,"status":"running"}}}`), &update); err != nil {
			t.Fatal(err)
		}
		journalTestSessionUpdate(t, relay, update)
		submitRelay(relay, "interrupted-turn", prompt("Write the requested documentation"))
		journalTestClaim(t, relay)
		relay = journalTestOpen(t, temp, session, "1.0.0")
		journalTestSessionConfigured(t, relay)
		recovered := journalTestClaim(t, relay)
		if len(recovered) != 1 {
			t.Fatalf("recovered %+v", recovered)
		}
		if journalTestJSON(t, recovered[0].Command) != journalTestJSON(t, prompt("Write the requested documentation")) {
			t.Fatalf("recovered command = %#v", recovered[0].Command)
		}
	}
}

// Hard-won: 1c150431b: recovery discarded requests never persisted by the harness.
func TestRestartContinuationMovesAMaximumRequestWhenTheDurableQueueIsNearFull(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	overhead := len(journalTestJSON(t, prompt("")))
	original := prompt(strings.Repeat("x", core.RelayCommandByteBudget-overhead))
	if size := len(journalTestJSON(t, original)); size != core.RelayCommandByteBudget {
		t.Fatalf("original request is %d bytes", size)
	}
	submitRelay(relay, "maximum-request", original)
	journalTestClaim(t, relay)
	// Each queued request is represented in receipt, dispatch, and queue.
	// Fill until one additional admitted-size payload would exceed the
	// snapshot's documented budget; moving the current turn must still fit.
	queued := 0
	for {
		if _, _, err := relay.submitCommand(fmt.Sprintf("queued-large-request-%d", queued), original); err != nil {
			break
		}
		queued++
	}
	if queued == 0 {
		t.Fatal("nothing was queued")
	}
	if size := len(journalTestJSON(t, relay.snapshot)); size <= core.RelaySnapshotByteBudget-3*core.RelayCommandByteBudget {
		t.Fatalf("snapshot is only %d bytes", size)
	}
	// Claim is durable, but ACP never received this request.
	for range 3 {
		relay = journalTestOpen(t, temp, session, "1.0.0")
		journalTestSessionConfigured(t, relay)
		recovered := journalTestClaim(t, relay)
		if len(recovered) != 1 {
			t.Fatalf("recovered %+v", recovered)
		}
		if err := core.EnsureSerializedBudget(recovered[0].Command, core.RelayCommandByteBudget, "stored recovery command"); err != nil {
			t.Fatal(err)
		}
		blocks := journalTestBlocksForCommand(t, recovered[0])
		if len(relay.snapshot.QueuedPrompts) != queued {
			t.Fatalf("%d queued prompts, want %d", len(relay.snapshot.QueuedPrompts), queued)
		}
		if journalTestJSON(t, journalTestPromptBlocks(t, recovered[0].Command)) != journalTestJSON(t, journalTestPromptBlocks(t, original)) {
			t.Fatal("the recovered request lost the original blocks")
		}
		if len(blocks) != 1 {
			t.Fatalf("%d recovered blocks", len(blocks))
		}
	}
}

// Hard-won: 1c150431b: a mailbox wake's task was missing from its restart continuation.
func TestRestartContinuationPreservesMailboxWakeContentWithoutRedeliveringItsEvents(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "1.0.0")
	journalTestSessionConfigured(t, relay)
	submitRelay(relay, "deliver-task", core.RelayCommandDeliverMailboxEvent{Event: core.MailboxEvent{
		Key:         "restart-task",
		Source:      "github",
		Wake:        true,
		CreatedAtMs: 1,
		Body: core.MailboxEventBodyPlainText{
			Text: strings.Repeat("<", 768*1024) + "Finish <the task> & preserve its instructions.",
		},
	}})
	claimed := journalTestClaim(t, relay)
	if _, wake := claimed[0].Command.(core.RelayCommandMailboxWake); !wake {
		t.Fatalf("claimed %#v", claimed[0].Command)
	}
	originalBlocks := core.DeepClone(journalTestPromptBlocks(t, claimed[0].Command))
	if size := len(journalTestJSON(t, originalBlocks)); size <= core.RelayEventByteBudget {
		t.Fatalf("original blocks are only %d bytes", size)
	}
	originalCommand := journalTestJSON(t, claimed[0].Command)
	deliveries := core.DeepClone(relay.snapshot.DeliveredMailboxEventKeys)
	for attempt := range 2 {
		relay = journalTestOpen(t, temp, session, "1.0.0")
		journalTestSessionConfigured(t, relay)
		recovered := journalTestClaim(t, relay)
		if len(recovered) != 1 {
			t.Fatalf("recovered %+v", recovered)
		}
		if journalTestJSON(t, journalTestPromptBlocks(t, recovered[0].Command)) != journalTestJSON(t, originalBlocks) {
			t.Fatal("the recovery lost the wake's content")
		}
		if blocks := journalTestBlocksForCommand(t, recovered[0]); len(blocks) != 1 {
			t.Fatalf("recovery must not replay the wake: %d blocks", len(blocks))
		}
		if journalTestJSON(t, recovered[0].Command) != originalCommand {
			t.Fatal("the recovered command differs from the original")
		}
		if attempt == 1 {
			journalTestComplete(t, relay, recovered[0].CommandID, journalTestEndTurn())
			if claimed := journalTestClaim(t, relay); len(claimed) != 0 {
				t.Fatalf("claimed %+v", claimed)
			}
		}
		if !reflect.DeepEqual(relay.snapshot.DeliveredMailboxEventKeys, deliveries) {
			t.Fatalf("deliveries = %v, want %v", relay.snapshot.DeliveredMailboxEventKeys, deliveries)
		}
		if len(relay.snapshot.PendingMailboxEvents) != 0 {
			t.Fatalf("pending mailbox events = %+v", relay.snapshot.PendingMailboxEvents)
		}
	}
}

// Hard-won: d2f26b4c6: a second mailbox claim wedged an inherited recovery ID.
func TestRestartContinuationKeepsAnExistingMailboxClaimAndLeavesNewMailPending(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "test")
	journalTestSessionConfigured(t, relay)
	event := func(key string) core.MailboxEvent {
		return core.MailboxEvent{Key: key, Source: "parent", Wake: true, CreatedAtMs: 1, Body: core.MailboxEventBodyParentMessage{Text: key}}
	}
	// Queue the prompt first so the mailbox rides its claim, rather than
	// starting a separate wake before the prompt is accepted.
	submitRelay(relay, "user-request", prompt("continue the task"))
	submitRelay(relay, "deliver-first", core.RelayCommandDeliverMailboxEvent{Event: event("first")})
	claimed := journalTestClaim(t, relay)
	originalContext := claimed[0].HiddenPromptContext
	submitRelay(relay, "deliver-second", core.RelayCommandDeliverMailboxEvent{Event: event("second")})
	relay = journalTestOpen(t, temp, session, "test")
	journalTestSessionConfigured(t, relay)
	recovery := journalTestClaim(t, relay)
	if len(recovery) != 1 {
		t.Fatalf("recovery %+v", recovery)
	}
	if !reflect.DeepEqual(recovery[0].HiddenPromptContext, originalContext) {
		t.Fatalf("hidden context = %v, want %v", recovery[0].HiddenPromptContext, originalContext)
	}
	if journalTestJSON(t, relay.snapshot.PendingMailboxEvents) != journalTestJSON(t, []core.MailboxEvent{event("second")}) {
		t.Fatalf("pending mailbox events = %+v", relay.snapshot.PendingMailboxEvents)
	}
	finishPrompt(relay, recovery[0].CommandID)
	wake := journalTestClaim(t, relay)
	command, isWake := wake[0].Command.(core.RelayCommandMailboxWake)
	if !isWake || journalTestJSON(t, command.Events) != journalTestJSON(t, []core.MailboxEvent{event("second")}) {
		t.Fatalf("wake = %#v", wake[0].Command)
	}
}

// Hard-won: 3b487762f: removing a queued recovery orphaned its inherited mailbox context.
func TestRemovingAQueuedRestartContinuationReleasesItsMailboxContext(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "test")
	journalTestSessionConfigured(t, relay)
	submitRelay(relay, "user-request", prompt("continue the task"))
	submitRelay(relay, "deliver-first", core.RelayCommandDeliverMailboxEvent{Event: core.MailboxEvent{
		Key:         "first",
		Source:      "parent",
		Wake:        true,
		CreatedAtMs: 1,
		Body:        core.MailboxEventBodyParentMessage{Text: "first"},
	}})
	journalTestClaim(t, relay)
	// Reopen without readiness, so the recovery stays queued.
	relay = journalTestOpen(t, temp, session, "test")
	recovery := relay.snapshot.QueuedPrompts[0].CommandID
	if !core.IsRestartContinuation(recovery) {
		t.Fatalf("%s is not a restart continuation", recovery)
	}
	if _, found := relay.snapshot.MailboxPromptContexts[recovery]; !found {
		t.Fatal("the recovery holds no mailbox context")
	}
	submitRelay(relay, "remove-recovery", core.RelayCommandRemoveQueuedPrompt{QueuedCommandID: recovery})
	if len(relay.snapshot.QueuedPrompts) != 0 || len(relay.snapshot.MailboxPromptContexts) != 0 {
		t.Fatalf("queued %+v, contexts %v", relay.snapshot.QueuedPrompts, relay.snapshot.MailboxPromptContexts)
	}
}

// Hard-won: d2f26b4c6: recovery metadata could exceed a completely full snapshot.
func TestRestartContinuationReportsQueueExhaustionWithoutWedgingOpen(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "test")
	large := prompt(strings.Repeat("x", core.RelayCommandByteBudget-len(journalTestJSON(t, prompt("")))))
	submitRelay(relay, "interrupted-full", large)
	journalTestClaim(t, relay)
	for n := 0; ; n++ {
		if _, _, err := relay.submitCommand(fmt.Sprintf("queued-full-%d", n), large); err != nil {
			break
		}
	}
	projectedSize := func(length int) int {
		commandID := "fill-remainder"
		event := journalTestSignedEvent(t, core.RelayEvent{
			Format:       core.RelayEventFormatV2,
			Ordinal:      relay.LatestOrdinal() + 1,
			RecordedAtMs: core.EpochMillis(),
			CommandID:    &commandID,
			Observation: core.RelayObservationCommandQueued{
				CommandID:   "fill-remainder",
				Command:     prompt(strings.Repeat("z", length)),
				CreatedAtMs: core.EpochMillis(),
			},
		})
		snapshot := relay.snapshot.Clone()
		if err := core.ApplyRelayEvent(&snapshot, &event); err != nil {
			t.Fatal(err)
		}
		return len(journalTestJSON(t, snapshot))
	}
	remaining := core.RelaySnapshotByteBudget - projectedSize(0)
	// The accepted command is stored in receipt, dispatch, and queue.
	submitRelay(relay, "fill-remainder", prompt(strings.Repeat("z", remaining/3)))
	if slack := core.RelaySnapshotByteBudget - len(journalTestJSON(t, relay.snapshot)); slack >= 3 {
		t.Fatalf("the snapshot still has %d bytes free", slack)
	}
	relay = journalTestOpen(t, temp, session, "test")
	if !journalTestAnyEvent(relay, func(observation core.RelayObservation) bool {
		warning, ok := observation.(core.RelayObservationWarning)
		return ok && strings.Contains(warning.Message, "could not be resumed automatically") &&
			strings.Contains(warning.Message, "queue is full")
	}) {
		t.Fatal("queue exhaustion was not reported")
	}
	if journalTestHasRestartContinuation(relay) {
		t.Fatal("a continuation was recorded despite the full queue")
	}
	journalTestOpen(t, temp, session, "test")
}

// Hard-won: 1d399ac64: a transfer rejected by the operational-state budget wedged recovery.
func TestRestartContinuationReportsOperationalQueueExhaustion(t *testing.T) {
	temp := t.TempDir()
	relay := journalTestOpen(t, temp, session, "test")
	journalTestAppend(t, relay, "original-turn", core.RelayObservationCommandQueued{
		CommandID:   "original-turn",
		Command:     prompt("finish task"),
		CreatedAtMs: core.EpochMillis(),
	})
	// A compact start timestamp keeps the new queue envelope larger than the
	// active entry even with the shorter generation/root recovery ID.
	journalTestRecord(t, relay, core.RelayObservationCommandStarted{CommandID: "original-turn", StartedAtMs: 0})
	journalTestClaim(t, relay)
	relay.ClearAcpReadiness()
	for index := 0; ; index++ {
		key := fmt.Sprintf("fill-state-%d", index)
		trial := relay.snapshot.Clone()
		trial.Config[key] = ""
		remaining := core.RelayStateByteBudget - len(journalTestJSON(t, trial.OperationalState())) - 1
		length := min(remaining, 256*1024)
		journalTestRecord(t, relay, core.RelayObservationConfigurationUpdated{Key: key, Value: strings.Repeat("z", length)})
		if length == remaining {
			break
		}
	}
	if _, err := relay.RecordCommandInterrupted("original-turn", core.OutcomeReasonWorkerRestarted, "worker restarted"); err != nil {
		t.Fatal(err)
	}
	if journalTestHasRestartContinuation(relay) {
		t.Fatal("a continuation was recorded despite the full operational state")
	}
	if !journalTestAnyEvent(relay, func(observation core.RelayObservation) bool {
		warning, ok := observation.(core.RelayObservationWarning)
		return ok && strings.Contains(warning.Message, "queue is full")
	}) {
		t.Fatal("queue exhaustion was not reported")
	}
	journalTestOpen(t, temp, session, "test")
}

// Hard-won: d2f26b4c6: adding a note changed slash-command routing after restart.
func TestRestartContinuationPreservesSlashCommandRouting(t *testing.T) {
	for _, text := range []string{"/compact", "/review changes"} {
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "test")
		submitRelay(relay, "slash-command", prompt(text))
		journalTestClaim(t, relay)
		relay = journalTestOpen(t, temp, session, "test")
		journalTestSessionConfigured(t, relay)
		recovery := journalTestClaim(t, relay)
		blocks := journalTestBlocksForCommand(t, recovery[0])
		if journalTestJSON(t, blocks) != journalTestJSON(t, journalTestPromptBlocks(t, prompt(text))) {
			t.Fatalf("%s: recovery blocks = %s", text, journalTestJSON(t, blocks))
		}
		if !core.PromptIsSlashCommand(blocks) {
			t.Fatalf("%s no longer routes as a slash command", text)
		}
	}
}
