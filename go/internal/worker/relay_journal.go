// Port of mj-worker/src/relay/journal.rs.
//
// Durable storage for the relay: the on-disk journal of sealed and active
// segments, snapshot persistence, restart recovery, and the parts of
// DurableRelay that make an event or a snapshot durable before it is
// acknowledged to a caller.

package worker

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// retainedTerminalCommands is the number of terminal command ids kept past
// journal acknowledgement so a retried submit with a known id is answered
// from the ledger instead of re-run.
const retainedTerminalCommands = core.RelayRetryIDRetention

func nonemptyJournalFile(path string) (bool, error) {
	info, err := os.Stat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("inspect %s: %w", path, err)
	}
	return info.Size() > 0, nil
}

// openOwnerOnly is Rust's owner_only_create_options: journal segments hold
// the full session transcript; create them owner-only the way credentials and
// checkpoint archives are created. The mode applies on Unix; Windows ignores
// it as Rust does.
func openOwnerOnly(path string, flag int) (*os.File, error) {
	return os.OpenFile(path, flag, 0o600)
}

// RelayJournalSpan is one canonical, non-overlapping slice of the durable
// journal.
type RelayJournalSpan struct {
	path string
	// fileFirstOrdinal is the physical first ordinal in path; it may precede
	// afterOrdinal when a crash left an overlapping active/sealed copy.
	fileFirstOrdinal uint64
	// fileFirstPreviousDigest is present when this process has read the
	// segment. Sealed segment names carry their ordinal range, so startup
	// does not need to decompress old transcript history merely to discover
	// its layout.
	fileFirstPreviousDigest *string
	fileLastOrdinal         uint64
	fileLastDigest          *string
	// afterOrdinal: this canonical span contributes only ordinals greater
	// than this value.
	afterOrdinal uint64
}

// spanPreviousDigest is the digest before a span, as cached by its first
// record. A v1 record carries it in previous_digest; a v2 record carries
// none, so the boundary digest must be read from the event itself when it is
// needed.
func spanPreviousDigest(first *core.RelayEvent) *string {
	if first.PreviousDigest == "" {
		return nil
	}
	digest := first.PreviousDigest
	return &digest
}

// persistRelaySnapshot persists the snapshot without ever recreating the
// worker root. Session teardown deletes that directory while this daemon may
// still be alive, and a recreated root holding only a snapshot cannot be
// reopened: its frontier would run ahead of a journal that no longer exists.
func persistRelaySnapshot(root string, snapshot *core.RelaySnapshot) error {
	panic("unported: mj-worker/src/relay/journal.rs persist_relay_snapshot")
}

// openRelayJournal recovers the canonical journal spans under journal and the
// hot tail of recent events, applying any crash tail newer than the snapshot
// to snapshot.
func openRelayJournal(
	journal string,
	retainedThrough uint64,
	retainedDigest string,
	snapshotOrdinal uint64,
	snapshot *core.RelaySnapshot,
) ([]RelayJournalSpan, []core.RelayEvent, error) {
	panic("unported: mj-worker/src/relay/journal.rs open_relay_journal")
}

func sealActiveRelaySegment(journal string, metadata *RelayJournalSpan) error {
	panic("unported: mj-worker/src/relay/journal.rs seal_active_relay_segment")
}

func inspectRelayJournalFile(path string, repairPartialTail bool) (*RelayJournalSpan, error) {
	panic("unported: mj-worker/src/relay/journal.rs inspect_relay_journal_file")
}

func sealedRelayJournalMetadata(path string) (RelayJournalSpan, error) {
	panic("unported: mj-worker/src/relay/journal.rs sealed_relay_journal_metadata")
}

// sealByteLimitOverride replaces the active-segment seal threshold so tests
// build multi-segment journals from a handful of small records instead of
// megabytes. Rust keeps it per test thread; Go tests that set it must not run
// in parallel with one another. Production code never sets it.
var sealByteLimitOverride atomic.Pointer[uint64]

func relaySegmentByteLimit() uint64 {
	if limit := sealByteLimitOverride.Load(); limit != nil {
		return *limit
	}
	return core.RelaySegmentByteLimit
}

// relayJournalVisitor receives each record of a journal file and its encoded
// length. Returning stop=true ends the scan (Rust's ControlFlow::Break);
// false continues it.
type relayJournalVisitor func(event core.RelayEvent, encodedLen int) (stop bool, err error)

func visitRelayJournalFile(path string, mode JournalReadMode, visitor relayJournalVisitor) ([]RelayJournalGap, error) {
	panic("unported: mj-worker/src/relay/journal.rs visit_relay_journal_file")
}

// JournalReadMode is how a journal file is read when a record does not parse.
type JournalReadMode int

const (
	// JournalReadModeStrict is read-only and strict: an unparseable
	// terminated record aborts the scan. A torn tail (no trailing newline)
	// stops cleanly without touching the file.
	JournalReadModeStrict JournalReadMode = iota
	// JournalReadModeRepairTail is startup repair of the live active segment:
	// like Strict, but a torn tail is truncated to the last complete record.
	JournalReadModeRepairTail
	// JournalReadModeRecover is recovery: an unparseable terminated record is
	// skipped and recorded as a byte gap; the scan recovers every intact
	// record either side of it. A torn tail stops cleanly (never truncates —
	// recovery is read-only).
	JournalReadModeRecover
)

// withoutTailRepair: tail repair is meaningless for a compressed sealed
// segment (its bytes are gzip-framed and immutable), so drop it there while
// keeping recovery.
func (m JournalReadMode) withoutTailRepair() JournalReadMode {
	if m == JournalReadModeRepairTail {
		return JournalReadModeStrict
	}
	return m
}

// RelayJournalGap is a run of unrecoverable bytes skipped during Recover —
// one or more corrupt records. The surrounding good records are recovered;
// the caller correlates the byte range with ordinals from the events it did
// receive.
type RelayJournalGap struct {
	byteOffset uint64
	byteLen    int
}

type RelayJournalScan struct {
	truncateTo *uint64
	gaps       []RelayJournalGap
}

func visitRelayJournalReader(
	path string,
	reader *bufio.Reader,
	mode JournalReadMode,
	visitor relayJournalVisitor,
) (RelayJournalScan, error) {
	panic("unported: mj-worker/src/relay/journal.rs visit_relay_journal_reader")
}

func archiveStaleActiveRelayJournal(journal, active string, metadata *RelayJournalSpan) error {
	panic("unported: mj-worker/src/relay/journal.rs archive_stale_active_relay_journal")
}

func readRestoredRelaySeed(root string) (*core.RestoredRelaySeed, error) {
	path := core.RestoredRelaySeedPath(root)
	if _, err := os.Stat(path); err != nil {
		return nil, nil
	}
	bytes, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}
	var seed core.RestoredRelaySeed
	if err := json.Unmarshal(bytes, &seed); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	if err := seed.Validate(); err != nil {
		return nil, fmt.Errorf("validate %s: %w", path, err)
	}
	return &seed, nil
}

func (r *DurableRelay) appendRelayEvent(commandID *string, observation core.RelayObservation) (uint64, error) {
	panic("unported: mj-worker/src/relay/journal.rs append_relay_event")
}

// stagesSnapshot is whether appending this observation has to stage and
// persist a snapshot.
func (r *DurableRelay) stagesSnapshot(observation core.RelayObservation) bool {
	if r.stageSnapshotEveryAppend {
		return true
	}
	return core.ObservationChangesState(observation)
}

func (r *DurableRelay) persistSnapshot() error {
	if err := persistRelaySnapshot(r.root, &r.snapshot); err != nil {
		return err
	}
	r.unpersistedJournalBytes = 0
	return nil
}

// commitSnapshot adopts a staged snapshot only once it is durable. Every
// write of relay-state.json goes through here or persistSnapshot, so
// unpersistedJournalBytes == 0 means the file matches memory.
func (r *DurableRelay) commitSnapshot(nextSnapshot core.RelaySnapshot) error {
	if err := persistRelaySnapshot(r.root, &nextSnapshot); err != nil {
		return err
	}
	r.snapshot = nextSnapshot
	r.unpersistedJournalBytes = 0
	return nil
}

func (r *DurableRelay) sealActiveSegmentIfNeeded() error {
	panic("unported: mj-worker/src/relay/journal.rs seal_active_segment_if_needed")
}

func (r *DurableRelay) recordJournalAppend(active string, event *core.RelayEvent) {
	if count := len(r.journalSpans); count > 0 && r.journalSpans[count-1].path == active {
		// Rust debug-asserts span.fileLastOrdinal+1 == event.Ordinal; its
		// release build, like this one, does not check.
		span := &r.journalSpans[count-1]
		digest := event.Digest
		span.fileLastOrdinal = event.Ordinal
		span.fileLastDigest = &digest
		return
	}
	digest := event.Digest
	r.journalSpans = append(r.journalSpans, RelayJournalSpan{
		path:                    active,
		fileFirstOrdinal:        event.Ordinal,
		fileFirstPreviousDigest: spanPreviousDigest(event),
		fileLastOrdinal:         event.Ordinal,
		fileLastDigest:          &digest,
		afterOrdinal:            event.Ordinal - 1,
	})
}

func (r *DurableRelay) pushHotEvent(event core.RelayEvent) {
	if len(r.hotEvents) == core.RelayHotEventCapacity {
		r.hotEvents = r.hotEvents[1:]
	}
	r.hotEvents = append(r.hotEvents, event)
}

// invalidateReplayPlans announces that the journal's files no longer match
// any replay plan a reader captured earlier. Callers reading a page off the
// relay lock compare the generation to tell a stale plan from a real desync.
func (r *DurableRelay) invalidateReplayPlans() {
	r.journalGeneration++
}

func (r *DurableRelay) collectAcknowledgedJournalFiles(through uint64, includeRedundant bool) error {
	panic("unported: mj-worker/src/relay/journal.rs collect_acknowledged_journal_files")
}

func (r *DurableRelay) hasCollectableJournalFiles(through uint64) (bool, error) {
	panic("unported: mj-worker/src/relay/journal.rs has_collectable_journal_files")
}

func (r *DurableRelay) hasRedundantJournalFiles() (bool, error) {
	panic("unported: mj-worker/src/relay/journal.rs has_redundant_journal_files")
}

func (r *DurableRelay) garbageCollectRelayHistory() error {
	panic("unported: mj-worker/src/relay/journal.rs garbage_collect_relay_history")
}

// prunableCommandIDs lists ledger entries whose command is terminal at or
// below through, except for the newest retainedTerminalCommands and
// owner-pinned receipts.
//
// Their events are no longer retained, but a controller that lost the reply
// to a submit may still retry it with the same command id after
// acknowledging past its completion. Keeping a bounded tail of terminal ids
// lets that retry be answered with the original acceptance instead of
// running the command twice. Rust: DurableRelay::prunable_command_ids.
func prunableCommandIDs(snapshot *core.RelaySnapshot, through uint64) []string {
	panic("unported: mj-worker/src/relay/journal.rs prunable_command_ids")
}

func (r *DurableRelay) recoverNonterminalCommands() error {
	panic("unported: mj-worker/src/relay/journal.rs recover_nonterminal_commands")
}
