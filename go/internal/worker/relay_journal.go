// Port of mj-worker/src/relay/journal.rs.
//
// Durable storage for the relay: the on-disk journal of sealed and active
// segments, snapshot persistence, restart recovery, and the parts of
// DurableRelay that make an event or a snapshot durable before it is
// acknowledged to a caller.

package worker

import (
	"bufio"
	"cmp"
	"compress/gzip"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"unicode/utf8"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
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

// createOwnerOnlyEmpty creates (or truncates) path owner-only and syncs it,
// as Rust's owner_only_create_options().create(true).truncate(true)
// .write(true) followed by sync_all does.
func createOwnerOnlyEmpty(path string) error {
	file, err := openOwnerOnly(path, os.O_CREATE|os.O_TRUNC|os.O_WRONLY)
	if err != nil {
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	return file.Close()
}

// relayPathExists is Rust's Path::exists: false on any metadata error.
func relayPathExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// relayJournalIsGz is Rust's `path.extension() == Some("gz")`. Like Rust, a
// name that is only a leading dot and an extension (".gz") has none.
func relayJournalIsGz(path string) bool {
	name := filepath.Base(path)
	dot := strings.LastIndexByte(name, '.')
	return dot > 0 && name[dot+1:] == "gz"
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
	body, err := workerRuntimeMarshalJSON(snapshot)
	if err != nil {
		return err
	}
	if err := core.EnsureByteBudget(len(body), core.RelaySnapshotByteBudget, "relay snapshot"); err != nil {
		return err
	}
	return core.AtomicWriteExisting(filepath.Join(root, core.RelayStateFile), body)
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
	var paths []string
	if relayPathExists(journal) {
		entries, err := os.ReadDir(journal)
		if err != nil {
			return nil, nil, err
		}
		for _, entry := range entries {
			path := filepath.Join(journal, entry.Name())
			if entry.Name() == core.RelayActiveSegment || relayJournalIsGz(path) {
				paths = append(paths, path)
			}
		}
	}
	slices.Sort(paths)
	active := filepath.Join(journal, core.RelayActiveSegment)
	var files []RelayJournalSpan
	for _, path := range paths {
		if path == active {
			metadata, err := inspectRelayJournalFile(path, true)
			if err != nil {
				return nil, nil, err
			}
			if metadata != nil {
				files = append(files, *metadata)
			}
			continue
		}
		metadata, err := sealedRelayJournalMetadata(path)
		if err != nil {
			return nil, nil, err
		}
		files = append(files, metadata)
	}

	originalFrontiers := []struct {
		name    string
		ordinal uint64
		digest  string
	}{
		{"snapshot", snapshot.LatestOrdinal, snapshot.LatestDigest},
		{"acknowledgement", snapshot.AcknowledgedThrough, snapshot.AcknowledgedDigest},
		{"recovery floor", snapshot.RecoveryFloorOrdinal, snapshot.RecoveryFloorDigest},
	}
	for _, frontier := range originalFrontiers {
		if frontier.ordinal == retainedThrough && frontier.digest != retainedDigest {
			return nil, nil, fmt.Errorf("relay %s digest conflicts with retained frontier", frontier.name)
		}
	}

	// A crash can leave old sealed files after the durable ACK and the
	// replacement active file were installed. They cannot lower the cut.
	journalLatest := retainedThrough
	for _, file := range files {
		journalLatest = max(journalLatest, file.fileLastOrdinal)
	}
	previousOrdinal := retainedThrough
	var spans []RelayJournalSpan

	for previousOrdinal < journalLatest {
		if previousOrdinal == math.MaxUint64 {
			return nil, nil, errors.New("relay event ordinal exhausted")
		}
		nextOrdinal := previousOrdinal + 1
		// Rust's max_by_key over (file_last_ordinal, is_active) keeps the
		// last of equal maxima.
		var candidate *RelayJournalSpan
		for index := range files {
			file := &files[index]
			if file.fileFirstOrdinal > nextOrdinal || file.fileLastOrdinal <= previousOrdinal {
				continue
			}
			if candidate == nil || relayJournalCandidateOrder(file, candidate, active) >= 0 {
				candidate = file
			}
		}
		if candidate == nil {
			return nil, nil, fmt.Errorf("relay journal has a gap after event %d", previousOrdinal)
		}
		span := *candidate
		span.afterOrdinal = previousOrdinal
		previousOrdinal = candidate.fileLastOrdinal
		spans = append(spans, span)
	}

	if journalLatest < snapshotOrdinal {
		return nil, nil, fmt.Errorf("relay snapshot frontier %d is ahead of retained journal %d", snapshotOrdinal, journalLatest)
	}
	for _, frontier := range originalFrontiers {
		if frontier.ordinal > retainedThrough && frontier.ordinal > journalLatest {
			return nil, nil, fmt.Errorf("relay %s event %d is not retained", frontier.name, frontier.ordinal)
		}
	}

	// The durable snapshot already contains the current operational state.
	// Only a crash tail newer than that snapshot must be decompressed and
	// applied during startup; retained history is validated when a controller
	// actually requests it. This keeps worker readiness proportional to the
	// bounded snapshot lag instead of the lifetime transcript size.
	var hotEvents []core.RelayEvent
	recoveredOrdinal := snapshotOrdinal
	recoveredDigest := snapshot.LatestDigest
	for _, span := range spans {
		if span.fileLastOrdinal <= recoveredOrdinal {
			continue
		}
		boundary := recoveredOrdinal
		boundaryVerified := span.fileFirstOrdinal == journalSaturatingAddU64(boundary, 1)
		_, err := visitRelayJournalFile(span.path, JournalReadModeStrict, func(event core.RelayEvent, _ int) (bool, error) {
			if event.Ordinal < boundary {
				return false, nil
			}
			if event.Ordinal == boundary {
				if event.Digest != recoveredDigest {
					return false, fmt.Errorf("overlapping relay journal %s conflicts at event %d", span.path, event.Ordinal)
				}
				boundaryVerified = true
				return false, nil
			}
			if !boundaryVerified {
				return false, fmt.Errorf("overlapping relay journal %s does not contain boundary event %d", span.path, boundary)
			}
			if err := core.ValidateRelayEvent(recoveredOrdinal, recoveredDigest, &event); err != nil {
				return false, fmt.Errorf("validate relay journal recovery tail: %w", err)
			}
			// Replay lacks the process-local activity levels that accompanied
			// a state change. Preserve known idle history across warnings, but
			// do not manufacture a transition time after an interrupted save.
			if _, sessionUpdate := event.Observation.(core.RelayObservationSessionUpdate); sessionUpdate ||
				core.ObservationChangesState(event.Observation) {
				snapshot.IdleSinceMs = nil
				snapshot.ActivityWasIdle = nil
			}
			if err := core.ApplyRelayEvent(snapshot, &event); err != nil {
				return false, err
			}
			recoveredOrdinal = event.Ordinal
			recoveredDigest = event.Digest
			if len(hotEvents) == core.RelayHotEventCapacity {
				hotEvents = hotEvents[1:]
			}
			hotEvents = append(hotEvents, event)
			return false, nil
		})
		if err != nil {
			return nil, nil, err
		}
		if recoveredOrdinal != span.fileLastOrdinal {
			return nil, nil, fmt.Errorf("relay journal %s ended at event %d, expected %d", span.path, recoveredOrdinal, span.fileLastOrdinal)
		}
	}
	if len(hotEvents) < core.RelayHotEventCapacity {
		// Collected newest first: Rust pushes each event onto the front of a
		// deque.
		var newestFirst []core.RelayEvent
		for index := len(spans) - 1; index >= 0; index-- {
			span := spans[index]
			var segmentEvents []core.RelayEvent
			var previous *core.RelayEvent
			_, err := visitRelayJournalFile(span.path, JournalReadModeStrict, func(event core.RelayEvent, _ int) (bool, error) {
				// Validate each record by its own digest; between consecutive
				// records the v1 chain link is also enforced. The first record
				// read cold has no trusted predecessor, so it is validated on
				// its own digest alone (a v2 record carries no back-reference).
				if previous != nil {
					if err := core.ValidateRelayEvent(previous.Ordinal, previous.Digest, &event); err != nil {
						return false, fmt.Errorf("validate relay journal %s: %w", span.path, err)
					}
				} else if err := core.ValidateRelayEventSelf(&event); err != nil {
					return false, fmt.Errorf("validate relay journal %s: %w", span.path, err)
				}
				if event.Ordinal > span.afterOrdinal && event.Ordinal <= snapshot.LatestOrdinal {
					segmentEvents = append(segmentEvents, event)
				}
				previous = &event
				return false, nil
			})
			if err != nil {
				return nil, nil, err
			}
			for next := len(segmentEvents) - 1; next >= 0; next-- {
				newestFirst = append(newestFirst, segmentEvents[next])
				if len(newestFirst) == core.RelayHotEventCapacity {
					break
				}
			}
			if len(newestFirst) == core.RelayHotEventCapacity {
				break
			}
		}
		slices.Reverse(newestFirst)
		hotEvents = newestFirst
	}
	if snapshot.LatestOrdinal > retainedThrough {
		if len(hotEvents) == 0 {
			return nil, nil, fmt.Errorf("relay journal is missing snapshot frontier event %d", snapshot.LatestOrdinal)
		}
		latest := hotEvents[len(hotEvents)-1]
		if latest.Ordinal != snapshot.LatestOrdinal || latest.Digest != snapshot.LatestDigest {
			return nil, nil, fmt.Errorf("relay snapshot digest conflicts with journal event %d", snapshot.LatestOrdinal)
		}
	}

	// An active copy left behind by a crash may be fully covered by a sealed
	// span. Keep it as a zero-width canonical span when it reaches the current
	// frontier so future appends remain contiguous. Preserve a stale shorter
	// copy under an ignored name before replacing it: sealed filenames are
	// enough to assemble history lazily, but are not proof that deleting the
	// overlapping active data would be safe.
	activeIndex := slices.IndexFunc(files, func(file RelayJournalSpan) bool { return file.path == active })
	if activeIndex >= 0 && !slices.ContainsFunc(spans, func(span RelayJournalSpan) bool { return span.path == active }) {
		activeFile := files[activeIndex]
		if activeFile.fileLastOrdinal == previousOrdinal {
			activeFile.afterOrdinal = previousOrdinal
			spans = append(spans, activeFile)
		} else if activeFile.fileLastOrdinal < previousOrdinal {
			if err := archiveStaleActiveRelayJournal(journal, active, &activeFile); err != nil {
				return nil, nil, err
			}
		}
	}
	return spans, hotEvents, nil
}

// relayJournalCandidateOrder compares two files by the (file_last_ordinal,
// is_active) key a canonical span is chosen by.
func relayJournalCandidateOrder(a, b *RelayJournalSpan, active string) int {
	isActive := func(file *RelayJournalSpan) int {
		if file.path == active {
			return 1
		}
		return 0
	}
	return cmp.Or(cmp.Compare(a.fileLastOrdinal, b.fileLastOrdinal), cmp.Compare(isActive(a), isActive(b)))
}

func journalSaturatingAddU64(a, b uint64) uint64 {
	if a > math.MaxUint64-b {
		return math.MaxUint64
	}
	return a + b
}

func sealActiveRelaySegment(journal string, metadata *RelayJournalSpan) error {
	active := filepath.Join(journal, core.RelayActiveSegment)
	sealedName := fmt.Sprintf("segment-%020d-%020d.jsonl.gz", metadata.fileFirstOrdinal, metadata.fileLastOrdinal)
	temporary := filepath.Join(journal, sealedName+".new")
	destination := filepath.Join(journal, sealedName)
	file, err := openOwnerOnly(temporary, os.O_CREATE|os.O_TRUNC|os.O_WRONLY)
	if err != nil {
		return err
	}
	encoder := gzip.NewWriter(file)
	_, err = visitRelayJournalFile(active, JournalReadModeStrict, func(event core.RelayEvent, _ int) (bool, error) {
		encoded, err := workerRuntimeMarshalJSON(event)
		if err != nil {
			return false, err
		}
		if _, err := encoder.Write(append(encoded, '\n')); err != nil {
			return false, err
		}
		return false, nil
	})
	if err == nil {
		err = encoder.Close()
	}
	if err == nil {
		err = file.Sync()
	}
	if closeErr := file.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	if err := os.Rename(temporary, destination); err != nil {
		return err
	}
	// The sealed copy must be durable before the active segment is replaced.
	if err := core.SyncDirectory(journal); err != nil {
		return err
	}

	replacement := filepath.Join(journal, "active.jsonl.new")
	if err := createOwnerOnlyEmpty(replacement); err != nil {
		return err
	}
	if err := os.Rename(replacement, active); err != nil {
		return err
	}
	// From this point the live active path is empty. Publish the sealed path
	// in memory before the final directory sync so even a reported sync error
	// cannot make a subsequent retry append against stale file metadata.
	metadata.path = destination
	return core.SyncDirectory(journal)
}

func inspectRelayJournalFile(path string, repairPartialTail bool) (*RelayJournalSpan, error) {
	var first, previous *core.RelayEvent
	mode := JournalReadModeStrict
	if repairPartialTail {
		mode = JournalReadModeRepairTail
	}
	_, err := visitRelayJournalFile(path, mode, func(event core.RelayEvent, encodedLen int) (bool, error) {
		if err := core.EnsureByteBudget(encodedLen, core.RelayEventByteBudget, "relay event"); err != nil {
			return false, err
		}
		if previous != nil {
			if err := core.ValidateRelayEvent(previous.Ordinal, previous.Digest, &event); err != nil {
				return false, fmt.Errorf("validate relay journal %s: %w", path, err)
			}
		} else {
			if err := core.ValidateRelayEventSelf(&event); err != nil {
				return false, fmt.Errorf("validate relay journal %s: %w", path, err)
			}
			firstEvent := event
			first = &firstEvent
		}
		previous = &event
		return false, nil
	})
	if err != nil {
		return nil, err
	}
	if first == nil || previous == nil {
		return nil, nil
	}
	lastDigest := previous.Digest
	return &RelayJournalSpan{
		path:                    path,
		fileFirstOrdinal:        first.Ordinal,
		fileFirstPreviousDigest: spanPreviousDigest(first),
		fileLastOrdinal:         previous.Ordinal,
		fileLastDigest:          &lastDigest,
		afterOrdinal:            0,
	}, nil
}

func sealedRelayJournalMetadata(path string) (RelayJournalSpan, error) {
	name := filepath.Base(path)
	if !utf8.ValidString(name) {
		return RelayJournalSpan{}, fmt.Errorf("relay segment has a non-UTF-8 name: %s", path)
	}
	rangeText, named := strings.CutPrefix(name, "segment-")
	if named {
		rangeText, named = strings.CutSuffix(rangeText, ".jsonl.gz")
	}
	if !named {
		return RelayJournalSpan{}, fmt.Errorf("invalid sealed relay segment name %s", path)
	}
	firstText, lastText, found := strings.Cut(rangeText, "-")
	if !found {
		return RelayJournalSpan{}, fmt.Errorf("invalid sealed relay segment range %s", path)
	}
	first, err := parseRustU64(firstText)
	if err != nil {
		return RelayJournalSpan{}, fmt.Errorf("parse first ordinal from %s: %w", path, err)
	}
	last, err := parseRustU64(lastText)
	if err != nil {
		return RelayJournalSpan{}, fmt.Errorf("parse last ordinal from %s: %w", path, err)
	}
	if first == 0 || first > last {
		return RelayJournalSpan{}, fmt.Errorf("invalid sealed relay segment range %d-%d", first, last)
	}
	return RelayJournalSpan{
		path:             path,
		fileFirstOrdinal: first,
		fileLastOrdinal:  last,
		afterOrdinal:     0,
	}, nil
}

// parseRustU64 is Rust's `str::parse::<u64>`: ASCII digits with an optional
// leading '+', with Rust's error texts.
func parseRustU64(text string) (uint64, error) {
	if text == "" {
		return 0, errors.New("cannot parse integer from empty string")
	}
	digits := strings.TrimPrefix(text, "+")
	if digits == "" {
		return 0, errors.New("invalid digit found in string")
	}
	for _, character := range digits {
		if character < '0' || character > '9' {
			return 0, errors.New("invalid digit found in string")
		}
	}
	value, err := strconv.ParseUint(digits, 10, 64)
	if err != nil {
		return 0, errors.New("number too large to fit in target type")
	}
	return value, nil
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
	if !relayPathExists(path) {
		return nil, nil
	}
	if relayJournalIsGz(path) {
		file, err := os.Open(path)
		if err != nil {
			return nil, fmt.Errorf("open %s: %w", path, err)
		}
		defer file.Close()
		// flate2's GzDecoder reports a bad header on the first read; Go's
		// reader reports it here, under the same context.
		decoder, err := gzip.NewReader(bufio.NewReader(file))
		if err != nil {
			return nil, fmt.Errorf("read relay journal %s: %w", path, err)
		}
		// flate2's GzDecoder reads a single gzip member.
		decoder.Multistream(false)
		defer decoder.Close()
		scan, err := visitRelayJournalReader(path, bufio.NewReader(decoder), mode.withoutTailRepair(), visitor)
		if err != nil {
			return nil, err
		}
		return scan.gaps, nil
	}

	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", path, err)
	}
	scan, err := visitRelayJournalReader(path, bufio.NewReader(file), mode, visitor)
	file.Close()
	if err != nil {
		return nil, err
	}
	if scan.truncateTo != nil {
		torn, err := os.OpenFile(path, os.O_WRONLY, 0)
		if err != nil {
			return nil, fmt.Errorf("open torn relay journal %s: %w", path, err)
		}
		if err := torn.Truncate(int64(*scan.truncateTo)); err != nil {
			torn.Close()
			return nil, err
		}
		if err := torn.Sync(); err != nil {
			torn.Close()
			return nil, err
		}
		if err := torn.Close(); err != nil {
			return nil, err
		}
		if err := core.SyncDirectory(filepath.Dir(path)); err != nil {
			return nil, err
		}
	}
	return scan.gaps, nil
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
	var line []byte
	var completeBytes uint64
	var gaps []RelayJournalGap
	for {
		consumed, terminated, err := core.ReadBoundedLine(reader, &line, core.RelayEventByteBudget)
		if err != nil {
			return RelayJournalScan{}, fmt.Errorf("read relay journal %s: %w", path, err)
		}
		if consumed == 0 {
			return RelayJournalScan{gaps: gaps}, nil
		}
		if !terminated {
			// A line with no trailing newline is a partial or in-flight
			// append, never a committed event: appendRelayEvent writes each
			// record and its newline in a single call. Stop at the last
			// complete record instead of parsing the torn bytes (which fails
			// with an "EOF while parsing" error). Repair mode truncates the
			// torn tail; every other mode leaves the file untouched for the
			// writer to finish.
			scan := RelayJournalScan{gaps: gaps}
			if mode == JournalReadModeRepairTail {
				validLen := completeBytes
				scan.truncateTo = &validLen
			}
			return scan, nil
		}
		recordOffset := completeBytes
		if completeBytes > math.MaxUint64-uint64(consumed) {
			return RelayJournalScan{}, errors.New("relay journal length overflow")
		}
		completeBytes += uint64(consumed)
		if len(line) == 0 {
			continue
		}
		var event core.RelayEvent
		if err := json.Unmarshal(line, &event); err != nil {
			// A terminated record that will not parse is unrecoverable. In
			// recovery, skip it so its corruption cannot poison the intact
			// records around it; otherwise fail as before.
			if mode == JournalReadModeRecover {
				slog.Warn("skipping unparseable relay journal record during recovery",
					"journal", path, "byte_offset", recordOffset, "bytes", len(line), "error", err)
				gaps = append(gaps, RelayJournalGap{byteOffset: recordOffset, byteLen: len(line)})
				continue
			}
			return RelayJournalScan{}, fmt.Errorf("parse relay journal %s at byte offset %d: %w", path, recordOffset, err)
		}
		// A record that parses but whose bytes were altered fails to recompute
		// its own digest. In recovery that is corruption too — skip it so a
		// flipped byte cannot be served as a valid but wrong record, and so it
		// cannot poison its neighbours. Strict/RepairTail leave this to the
		// caller's own validation, as before.
		if mode == JournalReadModeRecover {
			if err := core.ValidateRelayEventSelf(&event); err != nil {
				slog.Warn("skipping relay journal record that failed self-validation during recovery",
					"journal", path, "byte_offset", recordOffset, "bytes", len(line), "error", err)
				gaps = append(gaps, RelayJournalGap{byteOffset: recordOffset, byteLen: len(line)})
				continue
			}
		}
		stop, err := visitor(event, len(line))
		if err != nil {
			return RelayJournalScan{}, err
		}
		if stop {
			return RelayJournalScan{gaps: gaps}, nil
		}
	}
}

func archiveStaleActiveRelayJournal(journal, active string, metadata *RelayJournalSpan) error {
	archived := filepath.Join(journal, fmt.Sprintf("stale-active-%020d-%020d.jsonl", metadata.fileFirstOrdinal, metadata.fileLastOrdinal))
	if relayPathExists(archived) {
		return fmt.Errorf("cannot preserve stale relay journal %s because %s already exists", active, archived)
	}
	if err := os.Rename(active, archived); err != nil {
		return fmt.Errorf("preserve stale relay journal %s as %s: %w", active, archived, err)
	}
	if err := core.SyncDirectory(journal); err != nil {
		return err
	}

	replacement := filepath.Join(journal, "active.jsonl.new")
	if err := createOwnerOnlyEmpty(replacement); err != nil {
		return err
	}
	if err := os.Rename(replacement, active); err != nil {
		return err
	}
	return core.SyncDirectory(journal)
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

// journalAppendFault is a failure a test injects into the next journal
// append (Rust's #[cfg(test)] JournalAppendFault).
type journalAppendFault struct {
	kind         journalAppendFaultKind
	failRollback bool
}

type journalAppendFaultKind int

const (
	journalAppendFaultPartialWrite journalAppendFaultKind = iota
	journalAppendFaultDirectorySync
)

func (r *DurableRelay) appendRelayEvent(commandID *string, observation core.RelayObservation) (uint64, error) {
	if r.journalAppendFailure != nil {
		return 0, fmt.Errorf("relay journal appends disabled after failed rollback: %s", *r.journalAppendFailure)
	}
	// Every journal write passes here, so this is the one place that decides
	// whether a ready checkpoint barrier's hold survives it. What waited is
	// journaled first, which also keeps arrival order.
	if err := r.endCheckpointHoldFor(commandID, observation); err != nil {
		return 0, err
	}
	if r.snapshot.LatestOrdinal == math.MaxUint64 {
		return 0, errors.New("relay event ordinal exhausted")
	}
	ordinal := r.snapshot.LatestOrdinal + 1
	// Clamp before digesting so the recorded digest covers what was actually
	// written, and so recording an observation cannot fail on size alone.
	observation, err := core.ClampObservation(observation, core.RelayEventByteBudget-core.RelayEventEnvelopeReserve)
	if err != nil {
		return 0, err
	}
	// v2: self-describing records carry no chain link, so a corrupt record
	// can never invalidate the events after it. The frontier is still tracked
	// by latest_digest for cursor validation; it just is not folded into the
	// next record's digest.
	event := core.RelayEvent{
		Format:       core.RelayEventFormatV2,
		Ordinal:      ordinal,
		RecordedAtMs: core.EpochMillis(),
		Observation:  observation,
	}
	if commandID != nil {
		id := *commandID
		event.CommandID = &id
	}
	digest, err := core.RelayEventDigest(&event)
	if err != nil {
		return 0, err
	}
	event.Digest = digest
	// The same bytes are measured against the event budget and written to the
	// journal, so the encoding is done once.
	encoded, err := workerRuntimeMarshalJSON(event)
	if err != nil {
		return 0, fmt.Errorf("serialize relay event: %w", err)
	}
	if err := core.EnsureByteBudget(len(encoded), core.RelayEventByteBudget, "relay event"); err != nil {
		return 0, err
	}
	encoded = append(encoded, '\n')
	// A transcript observation leaves everything but the frontier alone, so
	// only a state-moving event pays for the staged snapshot copy and the two
	// budget serializations that validate it.
	stageSnapshot := r.stagesSnapshot(event.Observation)
	var staged *core.RelaySnapshot
	if stageSnapshot {
		nextSnapshot := r.snapshot.Clone()
		if err := core.ApplyRelayEvent(&nextSnapshot, &event); err != nil {
			return 0, err
		}
		if err := core.EnsureSerializedBudget(nextSnapshot, core.RelaySnapshotByteBudget, "relay snapshot"); err != nil {
			return 0, err
		}
		if err := core.EnsureSerializedBudget(nextSnapshot.OperationalState(), core.RelayStateByteBudget, "relay operational state"); err != nil {
			return 0, err
		}
		staged = &nextSnapshot
	}
	if err := r.sealActiveSegmentIfNeeded(); err != nil {
		return 0, err
	}
	journal := filepath.Join(r.root, core.RelayJournalDir)
	path := filepath.Join(journal, core.RelayActiveSegment)
	if err := r.commitJournalAppend(journal, path, ordinal, encoded); err != nil {
		return 0, err
	}
	if err := core.ReachTestHook("journal_append_before_snapshot_publication"); err != nil {
		return 0, err
	}

	// A delivered steer terminalizes its queued prompt. Capture the summary
	// before publishing the snapshot that drops terminal execution bodies.
	var summaryPrompt *string
	if deliveredID, delivered := transcript.DeliveredPromptCommandID(event.Observation); delivered {
		if dispatch, found := r.snapshot.Dispatches[deliveredID]; found {
			if blocks, isPrompt := core.RelayCommandPromptBlocks(dispatch.Command); isPrompt {
				texts := []string{}
				for _, block := range blocks {
					if block.Kind == acpschema.ContentBlockKindText && block.Text != nil {
						texts = append(texts, block.Text.Text)
					}
				}
				promptText := strings.Join(texts, "\n")
				summaryPrompt = &promptText
			}
		}
	}
	previousNativeSessionID := r.snapshot.NativeSessionID
	previousAssessment := r.snapshot.Assessment
	if staged != nil {
		r.snapshot = *staged
	} else if err := core.ApplyRelayEvent(&r.snapshot, &event); err != nil {
		// Applying is still what moves the frontier, so a misclassified
		// observation cannot silently lose its state change. It cannot fail
		// here either: this event was digested from this exact frontier,
		// which RelayEventDigest already validated.
		return 0, err
	}
	if !relayOptionalStringEqual(r.snapshot.NativeSessionID, previousNativeSessionID) {
		r.forgetAntigravityState()
	}
	if started, ok := event.Observation.(core.RelayObservationCommandStarted); ok &&
		r.snapshot.ActivePrompt != nil && r.snapshot.ActivePrompt.CommandID == started.CommandID {
		r.antigravityPromptStarted()
	}
	// The staged path replaced the snapshot with a deep copy and the
	// unstaged path leaves the assessment alone, so the old pointer still
	// holds the previous value.
	if r.classifiesTurns() && !reflect.DeepEqual(r.snapshot.Assessment, previousAssessment) {
		r.publishAssessmentDiagnostic()
	}
	if _, started := event.Observation.(core.RelayObservationHarnessTurnStarted); started &&
		r.snapshot.AutonomousHarnessTurn() != nil {
		r.turnContext.ObserveAutonomousTurnStart()
	}
	r.turnContext.ObserveRelay(event.Observation, summaryPrompt)
	authorizationChanged := false
	switch observed := event.Observation.(type) {
	case core.RelayObservationCommandStarted:
		authorizationChanged = true
	case core.RelayObservationCommandQueued:
		_, authorizationChanged = observed.Command.(core.RelayCommandSeedAssessmentContext)
	case core.RelayObservationCommandCompleted:
		switch observed.Outcome.(type) {
		case core.RelayCommandOutcomeSteered, core.RelayCommandOutcomePrompt, core.RelayCommandOutcomeContextCleared:
			authorizationChanged = true
		}
	case core.RelayObservationSessionUpdate:
		authorizationChanged = observed.Update.Kind == acpschema.SessionUpdateKindAgentMessageChunk
	case core.RelayObservationHarnessTurnSettled:
		authorizationChanged = !observed.PromptInFlight
	}
	if authorizationChanged {
		r.turnContext.SetAuthorizationContext(r.snapshot.AuthorizationContext())
	}
	invalidates := r.snapshot.Execution == core.RelayExecutionStateClosing ||
		r.snapshot.Execution == core.RelayExecutionStateClosed
	switch event.Observation.(type) {
	case core.RelayObservationHarnessTurnStarted, core.RelayObservationSessionRestarted, core.RelayObservationAgentInitialized:
		invalidates = true
	}
	if invalidates {
		r.turnContext.Invalidate()
	}
	idleChanged := r.refreshIdleClock(event.RecordedAtMs)
	r.recordJournalAppend(path, &event)
	r.pushHotEvent(event)
	if r.unpersistedJournalBytes > math.MaxInt-len(encoded) {
		r.unpersistedJournalBytes = math.MaxInt
	} else {
		r.unpersistedJournalBytes += len(encoded)
	}
	// Recovery replays journal events past the snapshot frontier, so a
	// streamed observation does not need its own snapshot write. Persisting on
	// every state move and once per bounded run of transcript bytes keeps that
	// replay short without paying two fsyncs per chunk. Whole-message Jev
	// evidence is reconstructed by journal replay. Its bounded growth is
	// checked above, but streaming still checkpoints at the existing byte
	// cadence rather than rewriting a snapshot per token.
	evidenceChunk := false
	if update, ok := event.Observation.(core.RelayObservationSessionUpdate); ok {
		evidenceChunk = update.Update.Kind == acpschema.SessionUpdateKindAgentMessageChunk
	}
	if (stageSnapshot && !evidenceChunk) || idleChanged || r.unpersistedJournalBytes >= core.RelaySnapshotLagByteLimit {
		if err := r.persistSnapshot(); err != nil {
			return 0, err
		}
	}
	if err := r.settleLocalQuota(); err != nil {
		return 0, err
	}
	if err := r.suppressChildAssessment(); err != nil {
		return 0, err
	}
	r.serviceLiveBaselines()
	return ordinal, nil
}

// relayOptionalStringEqual is Option<String> equality.
func relayOptionalStringEqual(left, right *string) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

// commitJournalAppend appends one encoded record to the active segment, or
// restores the segment's previous length when the append fails, so a retry
// never joins the next record to a torn one. A failed rollback leaves the
// boundary unknown and fences every later append.
func (r *DurableRelay) commitJournalAppend(journal, path string, ordinal uint64, encoded []byte) error {
	appendContext := func() string { return fmt.Sprintf("append relay event %d to %s", ordinal, path) }
	// The relay is the sole writer. Seek on a writable handle so the same
	// handle can truncate on Windows, where append mode forbids it.
	file, err := openOwnerOnly(path, os.O_CREATE|os.O_WRONLY)
	if err != nil {
		return fmt.Errorf("%s: open active segment: %w", appendContext(), err)
	}
	defer file.Close()
	originalLen, err := file.Seek(0, io.SeekEnd)
	if err != nil {
		return fmt.Errorf("%s: seek to active segment end: %w", appendContext(), err)
	}
	fault := r.journalAppendFault
	r.journalAppendFault = nil
	commit := func() error {
		if fault != nil && fault.kind == journalAppendFaultPartialWrite {
			if _, err := file.Write(encoded[:len(encoded)/2]); err != nil {
				return fmt.Errorf("write record prefix: %w", err)
			}
			return fmt.Errorf("write record: %w", syscall.ENOSPC)
		}
		if _, err := file.Write(encoded); err != nil {
			return fmt.Errorf("write record: %w", err)
		}
		if err := file.Sync(); err != nil {
			return fmt.Errorf("sync record: %w", err)
		}
		// An empty segment may be newly created or left by a failed first
		// append. Its first successful append must sync the directory.
		if originalLen == 0 {
			if fault != nil && fault.kind == journalAppendFaultDirectorySync {
				return fmt.Errorf("sync journal directory: %w", syscall.ENOSPC)
			}
			if err := core.SyncDirectory(journal); err != nil {
				return err
			}
		}
		return nil
	}
	if commitErr := commit(); commitErr != nil {
		rollback := func() error {
			if fault != nil && fault.kind == journalAppendFaultPartialWrite && fault.failRollback {
				return fmt.Errorf("truncate active segment: %w", syscall.EACCES)
			}
			if err := file.Truncate(originalLen); err != nil {
				return fmt.Errorf("truncate active segment: %w", err)
			}
			if err := file.Sync(); err != nil {
				return fmt.Errorf("sync truncated segment: %w", err)
			}
			return nil
		}
		err := fmt.Errorf("%s: %w", appendContext(), commitErr)
		if rollbackErr := rollback(); rollbackErr != nil {
			err = fmt.Errorf("rollback failed at length %d: %s: %w", originalLen, rollbackErr, err)
			failure := err.Error()
			r.journalAppendFailure = &failure
		}
		return err
	}
	return nil
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
	journal := filepath.Join(r.root, core.RelayJournalDir)
	active := filepath.Join(journal, core.RelayActiveSegment)
	if !relayPathExists(active) {
		return nil
	}
	info, err := os.Stat(active)
	if err != nil {
		return err
	}
	if uint64(info.Size()) < relaySegmentByteLimit() {
		return nil
	}
	index := slices.IndexFunc(r.journalSpans, func(span RelayJournalSpan) bool { return span.path == active })
	if index < 0 {
		return errors.New("active relay segment has data but no journal metadata")
	}
	// Sealing moves the active segment's events into a new file, so any
	// replay plan already captured stops describing the journal here.
	r.invalidateReplayPlans()
	return sealActiveRelaySegment(journal, &r.journalSpans[index])
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
	// Keep the boundary segment until its last event is acknowledged.
	// Segments rotate at RelaySegmentByteLimit; never copy a live suffix whose
	// size grows independently while the controller replays.
	journal := filepath.Join(r.root, core.RelayJournalDir)
	active := filepath.Join(journal, core.RelayActiveSegment)
	paths := map[string]struct{}{}
	for _, span := range r.journalSpans {
		if span.fileLastOrdinal > through {
			break
		}
		paths[span.path] = struct{}{}
	}
	if includeRedundant && relayPathExists(journal) {
		// Startup or a failed cleanup can leave redundant physical copies.
		// Their names suffice; acknowledged files need no decompression.
		entries, err := os.ReadDir(journal)
		if err != nil {
			return err
		}
		for _, entry := range entries {
			path := filepath.Join(journal, entry.Name())
			if !relayJournalIsGz(path) {
				continue
			}
			metadata, err := sealedRelayJournalMetadata(path)
			if err != nil {
				return err
			}
			if metadata.fileLastOrdinal <= through {
				paths[path] = struct{}{}
			}
		}
		if !slices.ContainsFunc(r.journalSpans, func(span RelayJournalSpan) bool { return span.path == active }) {
			nonempty, err := nonemptyJournalFile(active)
			if err != nil {
				return err
			}
			if nonempty {
				paths[active] = struct{}{}
			}
		}
	}
	if len(paths) == 0 {
		return nil
	}
	r.invalidateReplayPlans()
	ordered := make([]string, 0, len(paths))
	for path := range paths {
		ordered = append(ordered, path)
	}
	slices.Sort(ordered)
	removed := map[string]struct{}{}
	var result error
	for _, path := range ordered {
		if path == active {
			replacement := filepath.Join(journal, "active.jsonl.new")
			if err := createOwnerOnlyEmpty(replacement); err != nil {
				result = err
				break
			}
			if err := os.Rename(replacement, active); err != nil {
				result = err
				break
			}
		} else if err := os.Remove(path); err != nil {
			result = err
			break
		}
		removed[path] = struct{}{}
	}
	// Even partial failure must leave memory naming only existing canonical
	// files. No other reader can observe this decision until the owner
	// unlocks.
	r.journalSpans = slices.DeleteFunc(r.journalSpans, func(span RelayJournalSpan) bool {
		_, gone := removed[span.path]
		return gone
	})
	if result != nil {
		return result
	}
	return core.SyncDirectory(journal)
}

func (r *DurableRelay) hasCollectableJournalFiles(through uint64) (bool, error) {
	if len(r.journalSpans) > 0 && r.journalSpans[0].fileLastOrdinal <= through {
		return true, nil
	}
	if !r.journalCleanupPending {
		return false, nil
	}
	journal := filepath.Join(r.root, core.RelayJournalDir)
	if relayPathExists(journal) {
		entries, err := os.ReadDir(journal)
		if err != nil {
			return false, err
		}
		for _, entry := range entries {
			path := filepath.Join(journal, entry.Name())
			if !relayJournalIsGz(path) {
				continue
			}
			metadata, err := sealedRelayJournalMetadata(path)
			if err != nil {
				return false, err
			}
			if metadata.fileLastOrdinal <= through {
				return true, nil
			}
		}
		active := filepath.Join(journal, core.RelayActiveSegment)
		if !slices.ContainsFunc(r.journalSpans, func(span RelayJournalSpan) bool { return span.path == active }) {
			nonempty, err := nonemptyJournalFile(active)
			if err != nil {
				return false, err
			}
			if nonempty {
				return true, nil
			}
		}
	}
	return false, nil
}

func (r *DurableRelay) hasRedundantJournalFiles() (bool, error) {
	journal := filepath.Join(r.root, core.RelayJournalDir)
	if !relayPathExists(journal) {
		return false, nil
	}
	canonical := map[string]struct{}{}
	for _, span := range r.journalSpans {
		canonical[span.path] = struct{}{}
	}
	entries, err := os.ReadDir(journal)
	if err != nil {
		return false, err
	}
	for _, entry := range entries {
		path := filepath.Join(journal, entry.Name())
		if _, known := canonical[path]; relayJournalIsGz(path) && !known {
			return true, nil
		}
	}
	return false, nil
}

func (r *DurableRelay) garbageCollectRelayHistory() error {
	through := r.snapshot.RetainedThrough()
	journalFloor := r.snapshot.LatestOrdinal
	if len(r.journalSpans) > 0 {
		journalFloor = r.journalSpans[0].afterOrdinal
	}
	advancesFloor := through > journalFloor
	collectsJournal, err := r.hasCollectableJournalFiles(through)
	if err != nil {
		return err
	}
	includeRedundant := r.journalCleanupPending

	// The retained frontier this collection acts on has to be durable before
	// the journal loses the events below it: recovery reads that frontier
	// from the snapshot and replays forward from there.
	if (advancesFloor || collectsJournal) && r.unpersistedJournalBytes > 0 {
		if err := r.persistSnapshot(); err != nil {
			return err
		}
	}
	if advancesFloor {
		// A prepared replay must not serve the prefix after its logical
		// collection, even while a boundary file still contains those bytes.
		r.invalidateReplayPlans()
	}
	// Durable ACK authorizes unlinking fully covered files. A crash can leave
	// redundant acknowledged bytes, never discard unacknowledged work.
	if collectsJournal {
		r.journalCleanupPending = true
		if err := r.collectAcknowledgedJournalFiles(through, includeRedundant); err != nil {
			return err
		}
	}
	if len(r.journalSpans) > 0 && r.journalSpans[0].fileLastOrdinal > through {
		first := &r.journalSpans[0]
		first.afterOrdinal = max(first.afterOrdinal, through)
	}
	// A legacy rewrite can leave noncanonical sealed copies whose end is
	// still above this ACK. Keep considering them as later ACKs advance.
	redundant := false
	if includeRedundant {
		redundant, err = r.hasRedundantJournalFiles()
		if err != nil {
			return err
		}
	}
	r.journalCleanupPending = redundant
	// The pruned ledger reaches memory only after its own durable write
	// succeeds, so a transient failure cannot forget command IDs while the
	// daemon keeps serving retries. A catch-up ACK usually prunes nothing and
	// collects no files, and its own persist already made this snapshot
	// durable; writing it again would cost two more fsyncs for a file that
	// would not change.
	prunable := prunableCommandIDs(&r.snapshot, through)
	if len(prunable) > 0 || r.unpersistedJournalBytes > 0 {
		nextSnapshot := r.snapshot.Clone()
		for _, commandID := range prunable {
			delete(nextSnapshot.HandledCommands, commandID)
			delete(nextSnapshot.Dispatches, commandID)
		}
		if err := r.commitSnapshot(nextSnapshot); err != nil {
			return err
		}
	}
	// A fresh slice: a reader may still hold the old one.
	retained := make([]core.RelayEvent, 0, len(r.hotEvents))
	for _, event := range r.hotEvents {
		if event.Ordinal > through {
			retained = append(retained, event)
		}
	}
	r.hotEvents = retained
	return nil
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
	// Protect legacy receipts in place until the daemon attributes them;
	// copying their IDs could overflow an already full persisted snapshot.
	if snapshot.LegacyCommandRetention {
		return nil
	}
	pinned := map[string]struct{}{}
	for _, commandIDs := range snapshot.CommandRetention {
		for _, commandID := range commandIDs {
			pinned[commandID] = struct{}{}
		}
	}
	type terminalCommand struct {
		ordinal   uint64
		commandID string
	}
	var terminal []terminalCommand
	for commandID, handled := range snapshot.HandledCommands {
		if _, held := pinned[commandID]; held {
			continue
		}
		if handled.TerminalOrdinal != nil && *handled.TerminalOrdinal <= through {
			terminal = append(terminal, terminalCommand{ordinal: *handled.TerminalOrdinal, commandID: commandID})
		}
	}
	slices.SortFunc(terminal, func(a, b terminalCommand) int {
		return cmp.Or(cmp.Compare(b.ordinal, a.ordinal), cmp.Compare(b.commandID, a.commandID))
	})
	if len(terminal) <= retainedTerminalCommands {
		return nil
	}
	var prunable []string
	for _, entry := range terminal[retainedTerminalCommands:] {
		// Work tracking and login recovery still own the latest turn's
		// receipt even when controls have pushed it beyond the retry tail.
		if snapshot.TurnCompletion != nil && snapshot.TurnCompletion.CommandID == entry.commandID {
			continue
		}
		// Startup still consumes these receipts after /clear resets
		// native-use evidence; selector failures are consumed too.
		if slices.Contains(snapshot.InitialCommandIDs, entry.commandID) {
			continue
		}
		prunable = append(prunable, entry.commandID)
	}
	return prunable
}

// acceptedCommandIDs lists the dispatches that match keep, by the ordinal at
// which each was accepted (then command id), skipping any without a ledger
// entry.
func (r *DurableRelay) acceptedCommandIDs(keep func(commandID string, dispatch core.RelayDispatchRecord) bool) []string {
	type accepted struct {
		ordinal   uint64
		commandID string
	}
	var commands []accepted
	for commandID, dispatch := range r.snapshot.Dispatches {
		if !keep(commandID, dispatch) {
			continue
		}
		if handled, found := r.snapshot.HandledCommands[commandID]; found {
			commands = append(commands, accepted{ordinal: handled.AcceptedOrdinal, commandID: commandID})
		}
	}
	slices.SortFunc(commands, func(a, b accepted) int {
		return cmp.Or(cmp.Compare(a.ordinal, b.ordinal), cmp.Compare(a.commandID, b.commandID))
	})
	ids := make([]string, len(commands))
	for index, command := range commands {
		ids[index] = command.commandID
	}
	return ids
}

func relayDispatchSettled(state core.RelayDispatchState) bool {
	return state == core.RelayDispatchStateCompleted ||
		state == core.RelayDispatchStateRejected ||
		state == core.RelayDispatchStateInterrupted
}

func (r *DurableRelay) recoverNonterminalCommands() error {
	relayLocal := r.acceptedCommandIDs(func(commandID string, dispatch core.RelayDispatchRecord) bool {
		return (core.RelayCommandIsRelayLocal(dispatch.Command) || r.isQueuedRecoveryCancel(commandID)) &&
			!relayDispatchSettled(dispatch.State)
	})
	for _, commandID := range relayLocal {
		if err := r.finishRelayLocalCommand(commandID); err != nil {
			return err
		}
	}

	// Checkpoint barriers are controller-owned coordination commands. A
	// restarted relay has no owner that can complete them, regardless of
	// whether they were merely accepted, started, or already ready.
	ownerlessBarriers := r.acceptedCommandIDs(func(_ string, dispatch core.RelayDispatchRecord) bool {
		_, barrier := dispatch.Command.(core.RelayCommandBeginCheckpoint)
		return barrier && !relayDispatchSettled(dispatch.State)
	})
	for _, commandID := range ownerlessBarriers {
		// The transcript does not show this; the log and the journal do.
		slog.Info("relay restarted without the controller that owned the checkpoint barrier", "command_id", commandID)
		if _, err := r.RecordCommandInterrupted(
			commandID,
			core.OutcomeReasonOwnerLostOnRestart,
			"relay restarted without the controller that owned the checkpoint barrier",
		); err != nil {
			return err
		}
	}

	inFlight := r.acceptedCommandIDs(func(_ string, dispatch core.RelayDispatchRecord) bool {
		return dispatch.State == core.RelayDispatchStateInFlight
	})
	restoredClose := false
	for _, commandID := range inFlight {
		dispatch := r.snapshot.Dispatches[commandID]
		if _, closing := dispatch.Command.(core.RelayCommandClose); closing {
			// Closing an already-closed session is idempotent. Preserve the
			// durable close intent across a relay process restart.
			dispatch.State = core.RelayDispatchStatePending
			r.snapshot.Dispatches[commandID] = dispatch
			restoredClose = true
			continue
		}
		if shell, running := dispatch.Command.(core.RelayCommandRunUserShell); running {
			message := "worker restarted while the shell command was running; it was not replayed"
			if _, err := r.RecordCommandCompleted(commandID, core.RelayCommandOutcomeUserShell{
				Result: core.UserShellResult{
					Command: shell.Command,
					Status:  core.UserShellStatusInterrupted,
					Error:   &message,
				},
			}); err != nil {
				return err
			}
			continue
		}
		// Applying this interruption records that an in-flight prompt may
		// have reached ACP before the runtime's use notification was persisted
		// (see the CommandInterrupted arm of ApplyRelayEvent).
		if _, err := r.RecordCommandInterrupted(
			commandID,
			core.OutcomeReasonWorkerRestarted,
			"relay restarted while the ACP command was in flight; it was not replayed",
		); err != nil {
			return err
		}
	}
	if restoredClose {
		return r.persistSnapshot()
	}
	return nil
}
