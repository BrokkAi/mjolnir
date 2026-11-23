// Port of mj-worker/src/relay/journal_chaos.rs.
//
// Property-based chaos tests for the relay journal's recoverability.
//
// Each case builds a real on-disk journal, injects one randomized fault, and
// asserts the recovery contract: every record that is not actively corrupted
// is recovered, exactly the corrupt records are reported as gaps, no corrupt
// record is ever served as valid, and ordinals stay monotonic. Rust draws the
// faults with proptest; Go draws them from a seeded generator and reports
// the seed and case of any failure so it can be reproduced.
//
// Fully hermetic: every journal lives in a t.TempDir(); nothing touches the
// user's real data or config directories.

package worker

import (
	"encoding/json"
	"fmt"
	"math"
	"math/rand/v2"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// chaosRecord is an ordinal and its digest — the ground truth for a
// recovered record.
type chaosRecord struct {
	ordinal uint64
	digest  string
}

// chaosCases is proptest's case count: PROPTEST_CASES when set, otherwise
// defaultCases.
func chaosCases(defaultCases int) int {
	if value, ok := os.LookupEnv("PROPTEST_CASES"); ok {
		if cases, err := strconv.Atoi(value); err == nil {
			return cases
		}
	}
	return defaultCases
}

// chaosSeed is the seed of a test's generator: PROPTEST_SEED when set,
// otherwise a fixed per-test value so runs are reproducible.
func chaosSeed(name string) uint64 {
	if value, ok := os.LookupEnv("PROPTEST_SEED"); ok {
		if seed, err := strconv.ParseUint(value, 10, 64); err == nil {
			return seed
		}
	}
	var seed uint64 = 1469598103934665603
	for _, character := range []byte(name) {
		seed = (seed ^ uint64(character)) * 1099511628211
	}
	return seed
}

// chaosRun runs body once per generated case.
func chaosRun(t *testing.T, defaultCases int, body func(t *testing.T, random *rand.Rand)) {
	seed := chaosSeed(t.Name())
	random := rand.New(rand.NewPCG(seed, seed^0x9e3779b97f4a7c15))
	for index := range chaosCases(defaultCases) {
		caseSeed := random.Uint64()
		t.Run(fmt.Sprintf("case-%d-seed-%d", index, caseSeed), func(t *testing.T) {
			body(t, rand.New(rand.NewPCG(caseSeed, caseSeed^0x9e3779b97f4a7c15)))
		})
	}
}

// chaosSizes is prop::collection::vec(low..high, minLen..maxLen).
func chaosSizes(random *rand.Rand, low, high, minLen, maxLen int) []int {
	sizes := make([]int, minLen+random.IntN(maxLen-minLen))
	for index := range sizes {
		sizes[index] = low + random.IntN(high-low)
	}
	return sizes
}

// chaosSealOverride sets the seal threshold for the rest of the test (Rust's
// SealOverride guard).
func chaosSealOverride(t *testing.T, limit uint64) {
	setSealByteLimitOverride(&limit)
	t.Cleanup(func() { setSealByteLimitOverride(nil) })
}

// buildActiveJournal builds a single-segment (active-only) journal of
// len(sizes) v2 records and returns the root plus the truth set in order.
// Sealing is disabled so every record lands in active.jsonl with a
// predictable layout.
func buildActiveJournal(t *testing.T, sizes []int) (string, []chaosRecord) {
	t.Helper()
	limit := uint64(math.MaxUint64)
	setSealByteLimitOverride(&limit)
	defer setSealByteLimitOverride(nil)
	temp := t.TempDir()
	var truth []chaosRecord
	relay := journalTestOpen(t, temp, session, "1.0.0")
	for _, size := range sizes {
		ordinal := journalTestWarning(t, relay, strings.Repeat("x", size))
		truth = append(truth, chaosRecord{ordinal: ordinal, digest: relay.LatestDigest()})
	}
	return temp, truth
}

func captureNewRecords(t *testing.T, relay *DurableRelay, truth *[]chaosRecord) {
	t.Helper()
	after, digest := uint64(0), core.RelayEventGenesisDigest
	if count := len(*truth); count > 0 {
		after, digest = (*truth)[count-1].ordinal, (*truth)[count-1].digest
	}
	for _, event := range journalTestEventsAfter(t, relay, after, digest) {
		*truth = append(*truth, chaosRecord{ordinal: event.Ordinal, digest: event.Digest})
	}
}

func appendWarning(t *testing.T, relay *DurableRelay, truth *[]chaosRecord, size int) {
	t.Helper()
	ordinal := journalTestWarning(t, relay, strings.Repeat("m", size))
	*truth = append(*truth, chaosRecord{ordinal: ordinal, digest: relay.LatestDigest()})
}

// chaosLine is the byte range (start, length including the newline) of one
// newline-terminated line.
type chaosLine struct {
	start, length int
}

type chaosActiveEvent struct {
	event core.RelayEvent
	line  chaosLine
}

func activeEvents(t *testing.T, path string) []chaosActiveEvent {
	t.Helper()
	contents := journalTestReadFile(t, path)
	var events []chaosActiveEvent
	for _, line := range lineRanges(contents) {
		var event core.RelayEvent
		if json.Unmarshal(contents[line.start:line.start+line.length-1], &event) == nil {
			events = append(events, chaosActiveEvent{event: event, line: line})
		}
	}
	return events
}

func journalFiles(t *testing.T, temp string) []string {
	t.Helper()
	journal, _ := journalTestPath(temp)
	entries, err := os.ReadDir(journal)
	if err != nil {
		t.Fatal(err)
	}
	var files []string
	for _, entry := range entries {
		files = append(files, filepath.Join(journal, entry.Name()))
	}
	slices.Sort(files)
	return files
}

func activePath(temp string) string {
	_, active := journalTestPath(temp)
	return active
}

// lineRanges lists the byte ranges of each newline-terminated line.
func lineRanges(contents []byte) []chaosLine {
	var lines []chaosLine
	start := 0
	for index, character := range contents {
		if character == '\n' {
			lines = append(lines, chaosLine{start: start, length: index - start + 1})
			start = index + 1
		}
	}
	return lines
}

// recoverJournal reads every record Recover yields, and the gaps it reports.
func recoverJournal(path string) ([]chaosRecord, int, error) {
	var recovered []chaosRecord
	gaps, err := visitRelayJournalFile(path, JournalReadModeRecover, func(event core.RelayEvent, _ int) (bool, error) {
		recovered = append(recovered, chaosRecord{ordinal: event.Ordinal, digest: event.Digest})
		return false, nil
	})
	if err != nil {
		return nil, 0, fmt.Errorf("recover read errored: %w", err)
	}
	return recovered, len(gaps), nil
}

func chaosStrictlyIncreasing(records []chaosRecord) bool {
	for index := 1; index < len(records); index++ {
		if records[index-1].ordinal >= records[index].ordinal {
			return false
		}
	}
	return true
}

// Whatever single fault hits the active segment, the Recover reader returns
// exactly the records the fault could not have destroyed, reports the
// corrupt ones as gaps, and never yields a record that is not in the truth set
// or that fails its own digest.
// Hard-won: 27e5da78: recovery returned valid-JSON records whose digest had been corrupted by a byte flip.
func TestRecoverReaderIsolatesASingleActiveFault(t *testing.T) {
	chaosRun(t, 128, func(t *testing.T, random *rand.Rand) {
		messageSizes := chaosSizes(random, 0, 48, 1, 12)
		faultKind := random.IntN(4)
		idxSeed := random.Uint64()
		byteSeed := random.Uint64()

		temp, truth := buildActiveJournal(t, messageSizes)
		active := activePath(temp)
		contents := journalTestReadFile(t, active)
		lines := lineRanges(contents)
		if len(lines) != len(truth) {
			t.Fatalf("one line per recorded event: %d lines, %d events", len(lines), len(truth))
		}
		count := len(truth)
		idx := int(idxSeed % uint64(count))

		// Apply the fault and compute the exact oracle (survivors + gap count).
		var expectedSurvivors []chaosRecord
		var expectedGaps int
		switch faultKind {
		case 0:
			// Flip a content byte of record idx (never its trailing newline).
			// Any flip either breaks the JSON or changes a digested field, so
			// record idx becomes corrupt and nothing else does.
			line := lines[idx]
			contentLen := max(line.length-1, 1)
			position := line.start + int(byteSeed%uint64(contentLen))
			faulted := slices.Clone(contents)
			faulted[position] ^= 0x40
			journalTestWriteFile(t, active, faulted)
			for index, record := range truth {
				if index != idx {
					expectedSurvivors = append(expectedSurvivors, record)
				}
			}
			expectedGaps = 1
		case 1:
			// Insert a terminated garbage line after record idx: one extra
			// corrupt record, every real record survives.
			insertAt := lines[idx].start + lines[idx].length
			faulted := slices.Clone(contents[:insertAt])
			faulted = append(faulted, `{"ordinal":0,"observation": BROKEN`+"\n"...)
			faulted = append(faulted, contents[insertAt:]...)
			journalTestWriteFile(t, active, faulted)
			expectedSurvivors, expectedGaps = slices.Clone(truth), 1
		case 2:
			// Torn append: a partial record with no newline. Not a committed
			// record, so it is not a gap and every real record survives.
			faulted := append(slices.Clone(contents), `{"ordinal":999,"partial`...)
			journalTestWriteFile(t, active, faulted)
			expectedSurvivors, expectedGaps = slices.Clone(truth), 0
		default:
			// Truncate in the middle of record idx: records fully before it
			// survive; idx and everything after are lost to a torn tail (no
			// gap — an unterminated tail is never a committed record).
			line := lines[idx]
			cut := line.start + max(line.length/2, 1)
			journalTestWriteFile(t, active, slices.Clone(contents[:cut]))
			expectedSurvivors, expectedGaps = slices.Clone(truth[:idx]), 0
		}

		recovered, gaps, err := recoverJournal(active)
		if err != nil {
			t.Fatal(err)
		}

		// No fabricated data: every recovered record is a real one.
		for _, record := range recovered {
			if !slices.Contains(truth, record) {
				t.Fatalf("recovered a record not in the truth set: %+v", record)
			}
		}
		// Ordinals are strictly increasing.
		if !chaosStrictlyIncreasing(recovered) {
			t.Fatalf("recovered ordinals must be strictly increasing: %+v", recovered)
		}
		// Exactly the survivors, exactly the expected gaps.
		if !slices.Equal(recovered, expectedSurvivors) {
			t.Fatalf("recovered %+v, want %+v", recovered, expectedSurvivors)
		}
		if gaps != expectedGaps {
			t.Fatalf("%d gaps, want %d", gaps, expectedGaps)
		}
	})
}

// Corruption is always detected, never fabricated: no record that passes
// self-validation is outside the truth set, and the corrupt record never
// passes as its original self. (The raw reader is a parser; digest validation
// is the caller's job, so it is applied here as real serve paths like
// readEventsAfter do.)
// Hard-won: 27e5da78: recovery returned valid-JSON records whose digest had been corrupted by a byte flip.
func TestCorruptionIsDetectedAndNeverFabricated(t *testing.T) {
	chaosRun(t, 128, func(t *testing.T, random *rand.Rand) {
		messageSizes := chaosSizes(random, 0, 48, 2, 10)
		idxSeed := random.Uint64()
		byteSeed := random.Uint64()

		temp, truth := buildActiveJournal(t, messageSizes)
		active := activePath(temp)
		contents := journalTestReadFile(t, active)
		lines := lineRanges(contents)
		idx := int(idxSeed % uint64(len(truth)))
		line := lines[idx]
		position := line.start + int(byteSeed%uint64(max(line.length-1, 1)))
		faulted := slices.Clone(contents)
		faulted[position] ^= 0x40
		journalTestWriteFile(t, active, faulted)

		var servedValid []chaosRecord
		// Strict aborts on a parse error; keep whatever it parsed before that.
		_, _ = visitRelayJournalFile(active, JournalReadModeStrict, func(event core.RelayEvent, _ int) (bool, error) {
			if core.ValidateRelayEventSelf(&event) == nil {
				servedValid = append(servedValid, chaosRecord{ordinal: event.Ordinal, digest: event.Digest})
			}
			return false, nil
		})

		for _, record := range servedValid {
			if !slices.Contains(truth, record) {
				t.Fatalf("a record that passed self-validation must be real: %+v", record)
			}
		}
		if slices.Contains(servedValid, truth[idx]) {
			t.Fatalf("the corrupt record must not validate as its original: %+v", truth[idx])
		}
	})
}

// Generated multi-segment histories exercise append, seal, acknowledge,
// checkpoint-prune, reopen, corrupt, truncate, delete, duplicate, and recover
// transitions. A fault may discard only unacknowledged tail data: a
// successful reopen must preserve the exact acknowledged cursor and may serve
// only genuine, strictly ordered records from the reference model.
func TestAcknowledgedFrontierSurvivesGeneratedMultisegmentFaults(t *testing.T) {
	chaosRun(t, 128, func(t *testing.T, random *rand.Rand) {
		prefixSizes := chaosSizes(random, 8, 96, 8, 24)
		tailSizes := chaosSizes(random, 8, 96, 4, 16)
		checkpointPrune := random.IntN(2) == 1
		ackSeed := random.Uint64()
		faultKind := random.IntN(6)

		chaosSealOverride(t, 384)
		temp := t.TempDir()
		relay := journalTestOpen(t, temp, session, "1.0.0")
		var truth []chaosRecord
		for _, size := range prefixSizes {
			appendWarning(t, relay, &truth, size)
		}

		sealed := 0
		for _, path := range journalFiles(t, temp) {
			if relayJournalIsGz(path) {
				sealed++
			}
		}
		if sealed < 2 {
			t.Fatal("generated history must cross multiple sealed segments")
		}

		var acknowledged chaosRecord
		if checkpointPrune {
			ready := readyCheckpoint(relay, "model-checkpoint")
			captureNewRecords(t, relay, &truth)
			acknowledgeRelay(relay, "model-ack", ready.Ordinal)
			submitRelay(relay, "model-checkpoint-complete", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "model-checkpoint"})
			captureNewRecords(t, relay, &truth)
			acknowledged = chaosRecord{ordinal: ready.Ordinal, digest: ready.Digest}
		} else {
			index := int(ackSeed % uint64(len(truth)+1))
			if index == 0 {
				acknowledged = chaosRecord{ordinal: 0, digest: core.RelayEventGenesisDigest}
			} else {
				acknowledged = truth[index-1]
				acknowledgeRelay(relay, "model-ack", acknowledged.ordinal)
			}
		}

		for _, size := range tailSizes {
			appendWarning(t, relay, &truth, size)
		}

		active := activePath(temp)
		lastUnacknowledged := func() (chaosActiveEvent, bool) {
			events := activeEvents(t, active)
			for index := len(events) - 1; index >= 0; index-- {
				if events[index].event.Ordinal > acknowledged.ordinal {
					return events[index], true
				}
			}
			return chaosActiveEvent{}, false
		}
		switch faultKind {
		case 0:
			// Corrupt the last unacknowledged record without changing the
			// newline boundary, so recovery either rejects the journal or
			// drops exactly damaged tail data.
			contents := journalTestReadFile(t, active)
			if found, ok := lastUnacknowledged(); ok {
				contentEnd := found.line.start + found.line.length - 1
				position := found.line.start + max(contentEnd-found.line.start-1, 0)/2
				contents[position] ^= 0x40
				journalTestWriteFile(t, active, contents)
				if found.event.Ordinal <= acknowledged.ordinal {
					t.Fatal("corrupted an acknowledged record")
				}
			}
		case 1:
			// Truncate only the unacknowledged tail record.
			contents := journalTestReadFile(t, active)
			if found, ok := lastUnacknowledged(); ok {
				journalTestWriteFile(t, active, contents[:found.line.start+max(found.line.length/2, 1)])
			}
		case 2:
			// Delete an unacknowledged sealed segment. A gap must fail closed;
			// if another copy covers it, recovery may proceed.
			for _, path := range journalFiles(t, temp) {
				if !relayJournalIsGz(path) {
					continue
				}
				if span, err := sealedRelayJournalMetadata(path); err == nil && span.fileFirstOrdinal > acknowledged.ordinal {
					if err := os.Remove(path); err != nil {
						t.Fatal(err)
					}
					break
				}
			}
		case 3:
			// A duplicated committed line models a stale active copy left
			// beside a sealed generation.
			contents := journalTestReadFile(t, active)
			if events := activeEvents(t, active); len(events) > 0 {
				last := events[len(events)-1].line
				duplicated := append(slices.Clone(contents), contents[last.start:last.start+last.length]...)
				journalTestWriteFile(t, active, duplicated)
			}
		case 4:
			// A torn append was never committed and is not a gap.
			contents := append(journalTestReadFile(t, active), `{"ordinal":999,"partial`...)
			journalTestWriteFile(t, active, contents)
		default:
			// A terminated corrupt record is recoverable as one gap and cannot
			// fabricate a valid event.
			contents := append(journalTestReadFile(t, active), `{"ordinal":999,"observation":BROKEN}`+"\n"...)
			journalTestWriteFile(t, active, contents)
		}

		reopened, err := DurableRelayOpen(temp, session, "1.0.0")
		if err != nil {
			return
		}
		if reopened.AcknowledgedThrough() != acknowledged.ordinal || reopened.AcknowledgedDigest() != acknowledged.digest {
			t.Fatalf("reopened acknowledged %d/%s, want %d/%s", reopened.AcknowledgedThrough(),
				reopened.AcknowledgedDigest(), acknowledged.ordinal, acknowledged.digest)
		}
		retained, err := reopened.EventsAfter(acknowledged.ordinal, acknowledged.digest)
		if err != nil {
			t.Fatalf("serve recovered suffix: %v", err)
		}
		var recovered []chaosRecord
		for _, event := range retained {
			recovered = append(recovered, chaosRecord{ordinal: event.Ordinal, digest: event.Digest})
		}
		if !chaosStrictlyIncreasing(recovered) {
			t.Fatalf("recovered records must stay strictly ordered: %+v", recovered)
		}
		for _, record := range recovered {
			if !slices.Contains(truth, record) {
				t.Fatalf("recovery fabricated %+v", record)
			}
		}
		next, err := reopened.RecordObservation(core.RelayObservationWarning{Message: "after-recovery"})
		if err != nil {
			t.Fatalf("append after recovery: %v", err)
		}
		if next <= acknowledged.ordinal {
			t.Fatalf("appended at %d, below the acknowledged %d", next, acknowledged.ordinal)
		}
	})
}
