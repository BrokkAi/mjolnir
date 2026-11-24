// Port of mj-import/src/import/muse.rs.
//
// Rust calls these functions by their module path (`muse::scan`); the Go
// names carry the `muse` prefix instead.

package mjimport

import (
	"fmt"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const museActiveReason = "Muse session has an active or incomplete turn"
const museUnavailableReason = "Muse session is corrupt or uses an unsupported format"
const museDuplicateReason = "Muse session id occurs in multiple session directories"
const maxMuseSessionFileBytes uint64 = 1024 * 1024 * 1024

type museCandidate struct {
	nativeSessionID string
	sessionPath     string
	modifiedAt      time.Time
	sizeBytes       uint64
}

type museSummary struct {
	nativeSessionID string
	cwd             string
	title           string
	active          bool
}

// museLocate is Rust's `muse::locate`: a Muse session under its native
// date/session directory layout.
func museLocate(sessionsRoot string, selection ClaudeSessionSelection) (LocatedNativeSession, error) {
	panic("unported: mj-import/src/import/muse.rs locate")
}

// museScan is Rust's `muse::scan`. It scans only
// `YYYY/MM/DD/<session-id>/session.jsonl` files directly below the sessions
// root. `subagent/` descendants are native child streams and are captured
// with their parent rather than listed as independent sessions.
func museScan(sessionsRoot string, report func(SessionScanProgress[NativeSessionListing])) error {
	panic("unported: mj-import/src/import/muse.rs scan")
}

// museValidateTranscript is Rust's `muse::validate_transcript`: it validates
// the selected stream's identity and lifecycle before importing it, and
// returns the session's workspace root.
func museValidateTranscript(path string, originalKey string) (string, error) {
	panic("unported: mj-import/src/import/muse.rs validate_transcript")
}

func museCandidates(sessionsRoot string) ([]museCandidate, error) {
	panic("unported: mj-import/src/import/muse.rs muse_candidates")
}

// readDirectories lists the real (not symlinked) directories in root.
func readDirectories(root string) ([]string, error) {
	panic("unported: mj-import/src/import/muse.rs read_directories")
}

func isDateComponent(path string, width int) bool {
	name, ok := importPathFileName(path)
	if !ok || len(name) != width {
		return false
	}
	for i := 0; i < len(name); i++ {
		if name[i] < '0' || name[i] > '9' {
			return false
		}
	}
	return true
}

// museSummaryFor is Rust's `muse_summary` (renamed: the type museSummary
// keeps its name).
func museSummaryFor(path string) (museSummary, error) {
	data, err := readMuseSessionFile(path)
	if err != nil {
		return museSummary{}, err
	}
	records, err := checkpoint.ReadRecords(data)
	if err != nil {
		return museSummary{}, fmt.Errorf("parse Muse session %s: %w", path, err)
	}
	return museSummaryFromRecords(path, records)
}

func readMuseSessionFile(path string) ([]byte, error) {
	panic("unported: mj-import/src/import/muse.rs read_muse_session_file")
}

func museSummaryFromRecords(path string, records []checkpoint.MuseRecord) (museSummary, error) {
	panic("unported: mj-import/src/import/muse.rs muse_summary_from_records")
}

// museIntentText is the prompt text of a `runtime.user_intent.accepted`
// record (a jsonvalue value); false when it has none.
func museIntentText(record any) (string, bool) {
	panic("unported: mj-import/src/import/muse.rs muse_intent_text")
}

// museRecentDirectories is Rust's `muse::recent_directories`.
func museRecentDirectories(home string, limit int, executor core.CommandExecutor) (NativeProjectSeed, error) {
	panic("unported: mj-import/src/import/muse.rs recent_directories")
}
