// Port of mj-worker/src/relay/checkpoint_hold_tests.rs.
//
// The worker holds its own journal writes while a checkpoint barrier holds
// its cut (campaign finding I2-3).

package worker

import (
	"encoding/json"
	"slices"
	"strings"
	"testing"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func holdNotification(t *testing.T, text string) acpschema.SessionUpdate {
	t.Helper()
	encoded, err := json.Marshal(map[string]any{"sessionUpdate": "session_info_update", "title": text})
	if err != nil {
		t.Fatal(err)
	}
	var update acpschema.SessionUpdate
	if err := json.Unmarshal(encoded, &update); err != nil {
		t.Fatal(err)
	}
	return update
}

func holdAgentOutput(text string) acpschema.SessionUpdate {
	return acpschema.SessionUpdate{
		Kind:              acpschema.SessionUpdateKindAgentMessageChunk,
		AgentMessageChunk: &acpschema.ContentChunk{Content: relayTextBlock(text)},
	}
}

func holdJournal(t *testing.T, relay *DurableRelay) []core.RelayEvent {
	t.Helper()
	events, err := relay.EventsAfter(0, core.RelayEventGenesisDigest)
	if err != nil {
		t.Fatal(err)
	}
	return events
}

func holdNoticeOrdinal(t *testing.T, relay *DurableRelay, text string) (uint64, bool) {
	t.Helper()
	for _, event := range holdJournal(t, relay) {
		if notice, ok := event.Observation.(core.RelayObservationNotice); ok && notice.Message == text {
			return event.Ordinal, true
		}
	}
	return 0, false
}

// holdNoticeAt is notice_ordinal(..).unwrap(): the notice must be journaled.
func holdNoticeAt(t *testing.T, relay *DurableRelay, text string) uint64 {
	t.Helper()
	ordinal, found := holdNoticeOrdinal(t, relay, text)
	if !found {
		t.Fatalf("notice %q is not journaled", text)
	}
	return ordinal
}

func holdTitleRecorded(t *testing.T, relay *DurableRelay, text string) bool {
	t.Helper()
	return slices.ContainsFunc(holdJournal(t, relay), func(event core.RelayEvent) bool {
		encoded, err := json.Marshal(event.Observation)
		if err != nil {
			t.Fatal(err)
		}
		return strings.Contains(string(encoded), text)
	})
}

func holdConfigured(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(root, session, "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "native-session"}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}); err != nil {
		t.Fatal(err)
	}
	return relay
}

func holdRecordNotice(t *testing.T, relay *DurableRelay, text string) {
	t.Helper()
	if _, err := relay.RecordObservation(core.RelayObservationNotice{Message: text}); err != nil {
		t.Fatal(err)
	}
}

// I2-3: the harness spoke after the close's cut and the worker refused the
// Close. Output that changes no work now waits, so the Close matches its cut,
// and a sealed relay drops what waited instead of writing it past the seal.
// Hard-won: 634af0a3: late harness output moved the journal past the daemon's close cut and made Close fail.
func TestACloseSealsItsCutAlthoughTheHarnessSpokeAfterIt(t *testing.T) {
	relay := holdConfigured(t, t.TempDir())
	cut := readyCheckpoint(relay, "close-barrier")

	if _, err := relay.RecordSessionUpdate(holdNotification(t, "a title chosen after the turn")); err != nil {
		t.Fatal(err)
	}
	holdRecordNotice(t, relay, "said after the cut")
	if !relay.WorkerWritesHeld() {
		t.Fatal("the worker's writes are held behind the cut")
	}
	if got := relay.OperationalState().LatestOrdinal; got != cut.Ordinal {
		t.Fatalf("latest ordinal = %d, want the cut %d", got, cut.Ordinal)
	}

	submitRelay(relay, "close-command", core.RelayCommandClose{BarrierCommandID: "close-barrier", Expected: cut})
	submitRelay(relay, "close-checkpoint-complete", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "close-barrier"})
	if relay.WorkerWritesHeld() {
		t.Fatal("the completed barrier ends the hold")
	}
	if holdTitleRecorded(t, relay, "a title chosen after the turn") {
		t.Fatal("a sealed relay drops what waited")
	}
	if _, found := holdNoticeOrdinal(t, relay, "said after the cut"); found {
		t.Fatal("a sealed relay drops the held notice")
	}
}

// A routine checkpoint releases its barrier after capture and the session
// goes on. What waited is journaled then, in the order it arrived.
func TestAReleasedBarrierJournalsWhatWaitedInOrder(t *testing.T) {
	relay := holdConfigured(t, t.TempDir())
	cut := readyCheckpoint(relay, "routine-barrier")
	for _, text := range []string{"first", "second", "third"} {
		holdRecordNotice(t, relay, text)
	}
	if got := relay.OperationalState().LatestOrdinal; got != cut.Ordinal {
		t.Fatalf("latest ordinal = %d, want the cut %d", got, cut.Ordinal)
	}

	released := submitRelay(relay, "routine-release", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "routine-barrier"})
	first, okFirst := holdNoticeOrdinal(t, relay, "first")
	second, okSecond := holdNoticeOrdinal(t, relay, "second")
	third, okThird := holdNoticeOrdinal(t, relay, "third")
	if !okFirst || !okSecond || !okThird {
		t.Fatal("every held notice is journaled after release")
	}
	if !(released < first && first < second && second < third) {
		t.Fatalf("released %d, notices %d %d %d", released, first, second, third)
	}
}

// A barrier whose controller disconnects ends the hold the same way.
func TestACancelledBarrierJournalsWhatWaited(t *testing.T) {
	relay := holdConfigured(t, t.TempDir())
	readyCheckpoint(relay, "abandoned-barrier")
	holdRecordNotice(t, relay, "waited")
	if _, found := holdNoticeOrdinal(t, relay, "waited"); found {
		t.Fatal("the notice waits behind the barrier")
	}
	if _, _, err := relay.CancelCheckpointBarrierOnDisconnect("abandoned-barrier"); err != nil {
		t.Fatal(err)
	}
	if _, found := holdNoticeOrdinal(t, relay, "waited"); !found {
		t.Fatal("the cancelled barrier journals what waited")
	}
}

// A turn the harness starts on its own during a routine barrier means the
// agent may be writing to the workspace while it is captured. The daemon
// abandons that archive when it sees the turn start past its cursor, so the
// turn must reach the journal at once, as it did before writes were held:
// the hold ends, what waited goes first, and then the turn.
func TestAHarnessTurnDuringARoutineBarrierEndsTheHold(t *testing.T) {
	for _, announced := range []bool{false, true} {
		relay := holdConfigured(t, t.TempDir())
		relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
		cut := readyCheckpoint(relay, "routine-barrier")
		holdRecordNotice(t, relay, "before the turn")
		if got := relay.OperationalState().LatestOrdinal; got != cut.Ordinal {
			t.Fatalf("latest ordinal = %d, want the cut %d", got, cut.Ordinal)
		}

		if announced {
			if err := relay.applyClaudeTaskFixture("silent-turn", true, true, false, false); err != nil {
				t.Fatal(err)
			}
		} else {
			if _, err := relay.RecordSessionUpdate(holdAgentOutput("working on my own")); err != nil {
				t.Fatal(err)
			}
		}

		if relay.WorkerWritesHeld() {
			t.Fatalf("announced %v: the turn ends the hold", announced)
		}
		started := relay.OperationalState().LastHarnessTurnStartedOrdinal
		if started == nil {
			t.Fatal("the harness turn is journaled")
		}
		turnStarted := *started
		if turnStarted <= cut.Ordinal {
			t.Fatalf("turn started at %d, cut %d", turnStarted, cut.Ordinal)
		}
		if before := holdNoticeAt(t, relay, "before the turn"); before >= turnStarted {
			t.Fatalf("held notice %d journaled after the turn %d", before, turnStarted)
		}
		// Nothing waits for this barrier any more, so the turn's output is
		// journaled as it arrives.
		holdRecordNotice(t, relay, "during the turn")
		if during := holdNoticeAt(t, relay, "during the turn"); during <= turnStarted {
			t.Fatalf("notice during the turn journaled at %d, turn %d", during, turnStarted)
		}
	}
}

// A question for a person cannot wait behind a barrier: nobody could see it
// to answer it. It ends the hold and is journaled at once.
func TestAQuestionDuringABarrierIsJournaledAtOnce(t *testing.T) {
	relay := holdConfigured(t, t.TempDir())
	cut := readyCheckpoint(relay, "question-barrier")
	holdRecordNotice(t, relay, "before the question")
	asked, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: core.ElicitationRequest{
		ID:      "question-1",
		Message: "May I continue?",
		Fields:  []core.ElicitationField{},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if asked <= cut.Ordinal {
		t.Fatalf("question journaled at %d, cut %d", asked, cut.Ordinal)
	}
	if before := holdNoticeAt(t, relay, "before the question"); before >= asked {
		t.Fatalf("held notice %d journaled after the question %d", before, asked)
	}
	if relay.WorkerWritesHeld() {
		t.Fatal("the question ends the hold")
	}
}
