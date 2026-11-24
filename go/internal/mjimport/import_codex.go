// Port of mj-import/src/import/codex.rs.

package mjimport

import (
	"fmt"
	"math"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// codexRolloutDepth is how many directory levels below `sessions` a rollout
// can sit. Codex writes
// `sessions/<year>/<month>/<day>/rollout-<timestamp>-<id>.jsonl`, so the
// year, month and day directories are the last ones worth reading.
//
// Both walks below stop there. Nothing is gained by going deeper, and a
// symlink in the tree can point at a directory of any size: reading all of it
// would turn one lookup into a scan of the whole disk.
const codexRolloutDepth = 3

// LocateCodexSession locates a Codex rollout exposed by its native
// interactive resume picker.
func LocateCodexSession(home string, selection CodexSessionSelection) (LocatedCodexSession, error) {
	panic("unported: mj-import/src/import/codex.rs locate_codex_session")
}

// locateUnindexedCodexSession finds the rollout a named session id points
// at, for the ids the listing does not show. The listing is a resume picker:
// it needs a thread Codex itself indexed with a preview, and it hides
// subagent and ephemeral rollouts. None of that applies once the user names a
// session, so this lookup takes the rollout whatever the picker would have
// made of it.
//
// A rollout reached through a symlink is still refused, because the archive
// step stores only regular files inside the harness home. That refusal is
// reported, so the caller can say why the named path was rejected instead of
// reporting the session as missing.
func locateUnindexedCodexSession(home string, sessionID string) (LocatedCodexSession, error) {
	panic("unported: mj-import/src/import/codex.rs locate_unindexed_codex_session")
}

// namedCodexWalk is the walk behind a Codex lookup by session id. Unlike the
// listing walk it follows symlinked directories, so a rollout that exists
// only behind one is refused with its reason instead of being reported as
// missing. visited holds the directories already walked, which stops a
// symlink loop; codexRolloutDepth bounds how much of any tree is read.
type namedCodexWalk struct {
	sessionID string
	// visited is a set of canonical directories.
	visited  map[string]struct{}
	found    []fileScanCandidate
	rejected []string
}

// walk walks one directory, depth levels below the sessions root.
// viaSymlink names the symlinked directory this one was reached through, if
// any (nil otherwise): nothing under it can be archived.
func (w *namedCodexWalk) walk(directory string, depth int, viaSymlink *string) error {
	panic("unported: mj-import/src/import/codex.rs NamedCodexWalk::walk")
}

// ListCodexSessions lists native Codex sessions newest first.
func ListCodexSessions(home string) ([]LocatedCodexSession, error) {
	sessions := []LocatedCodexSession{}
	err := ScanCodexSessions(home, NewNativeScanCache(), func(progress SessionScanProgress[LocatedCodexSession]) {
		if progress.Session != nil {
			sessions = append(sessions, *progress.Session)
		}
	})
	if err != nil {
		return nil, err
	}
	return sessions, nil
}

// ScanCodexSessions scans native Codex sessions newest first, reporting
// after every candidate file.
func ScanCodexSessions(
	home string,
	cache *NativeScanCache,
	report func(SessionScanProgress[LocatedCodexSession]),
) error {
	panic("unported: mj-import/src/import/codex.rs scan_codex_sessions")
}

// codexIndexedSessions reads Codex's own thread index; false when the home
// has none Mjolnir can read.
func codexIndexedSessions(home string) ([]LocatedCodexSession, bool, error) {
	return codexIndexedSessionsLimited(home, math.MaxInt)
}

func codexIndexedSessionsLimited(home string, limit int) ([]LocatedCodexSession, bool, error) {
	panic("unported: mj-import/src/import/codex.rs codex_indexed_sessions_limited")
}

// collectCodexCandidatePaths collects the rollouts of one directory depth
// levels below the sessions root, and of the levels below it a rollout can
// still sit in.
func collectCodexCandidatePaths(
	root string,
	depth int,
	nativeTitles map[string]string,
	candidates *[]fileScanCandidate,
) error {
	panic("unported: mj-import/src/import/codex.rs collect_codex_candidate_paths")
}

// codexRolloutIDFromPath is the thread UUID a rollout's file name ends in.
func codexRolloutIDFromPath(path string) (string, bool) {
	panic("unported: mj-import/src/import/codex.rs codex_rollout_id_from_path")
}

// codexSessionSummary is what one Codex rollout's own header says about it.
type codexSessionSummary struct {
	// Subagent is positive child ownership, independently of resume
	// eligibility.
	Subagent bool
	// Filtered is the resume picker's verdict: a subagent rollout or an
	// ephemeral thread. Importing a session the user named by id ignores
	// this.
	Filtered bool
	// Metadata is nil when no `session_meta` record in the header names a
	// session.
	Metadata    *codexSessionMetadata
	HistoryMode CodexHistoryMode
}

// codexSessionMetadataFor is Rust's `codex_session_metadata` (renamed: the
// type codexSessionMetadata keeps its name): the picker's view of one
// rollout, nil when the file is filtered out of the resume list or names no
// session at all.
func codexSessionMetadataFor(path string) (*codexSessionMetadata, error) {
	summary, err := codexSessionSummaryFor(path)
	if err != nil {
		return nil, err
	}
	if summary.Filtered {
		return nil, nil
	}
	return summary.Metadata, nil
}

// codexSessionSummaryFor is Rust's `codex_session_summary` (renamed: the
// type codexSessionSummary keeps its name).
func codexSessionSummaryFor(path string) (codexSessionSummary, error) {
	panic("unported: mj-import/src/import/codex.rs codex_session_summary")
}

func parseCodexHistoryMode(value string) (CodexHistoryMode, error) {
	switch value {
	case "legacy":
		return CodexHistoryModeLegacy, nil
	case "paginated":
		return CodexHistoryModePaginated, nil
	}
	return 0, fmt.Errorf("unsupported Codex history mode %s", core.RustDebugString(value))
}

// codexSourceIsInteractive reads a session_meta `source`; present is false
// when the record has none (Rust's None, distinct from a JSON null).
func codexSourceIsInteractive(source any, present bool) bool {
	if !present {
		// Older rollouts predate the source field and came from the TUI.
		return true
	}
	if source, ok := source.(string); ok {
		return source == "cli" || source == "vscode"
	}
	// Structured sources identify subagents. Other unexpected shapes are not
	// sessions offered by the normal interactive resume picker.
	return false
}

// codexNativeTitles maps session ids to their titles from Codex's history and
// session-name index.
func codexNativeTitles(home string) (map[string]string, error) {
	panic("unported: mj-import/src/import/codex.rs codex_native_titles")
}
