// Port of mj-import/src/import/kimi.rs.

package mjimport

import (
	"io/fs"
	"time"
)

// LocateKimiSession locates a Kimi session directory. Its on-disk
// `session_<uuid>` name is the native identifier required by Kimi ACP's
// `session/load`.
func LocateKimiSession(home string, selection KimiSessionSelection) (LocatedKimiSession, error) {
	panic("unported: mj-import/src/import/kimi.rs locate_kimi_session")
}

// locateNamedKimiSession finds the session directory a named id points at,
// for the ids the listing does not show. The listing follows Kimi's own
// index: a session it never indexed, one marked deleted there, and one the
// user archived are all left out. None of that applies once the user names a
// session, so this lookup reads the directory on disk whatever the index
// says about it.
//
// A directory reached through a symlink is still refused, because the
// archive step stores only files that sit directly inside the harness home.
// That refusal is reported, so the caller can say why the named path was
// rejected instead of reporting the session as missing.
func locateNamedKimiSession(home string, nativeSessionID string) (LocatedKimiSession, error) {
	panic("unported: mj-import/src/import/kimi.rs locate_named_kimi_session")
}

// kimiNamedSessionPaths is where a named Kimi session directory can sit:
// directly under `sessions`, or inside one workspace directory there.
func kimiNamedSessionPaths(sessions string, nativeSessionID string, rejected *[]string) ([]string, error) {
	panic("unported: mj-import/src/import/kimi.rs kimi_named_session_paths")
}

// kimiIndexedWorkDir is the working directory Kimi's own index recorded for
// one session, whatever else that index says about it.
func kimiIndexedWorkDir(home string, nativeSessionID string) (string, bool) {
	panic("unported: mj-import/src/import/kimi.rs kimi_indexed_work_dir")
}

// ListKimiSessions lists native Kimi sessions newest first.
func ListKimiSessions(home string) ([]LocatedKimiSession, error) {
	sessions := []LocatedKimiSession{}
	err := ScanKimiSessions(home, func(progress SessionScanProgress[LocatedKimiSession]) {
		if progress.Session != nil {
			sessions = append(sessions, *progress.Session)
		}
	})
	if err != nil {
		return nil, err
	}
	return sessions, nil
}

// ScanKimiSessions scans native Kimi sessions newest first, reporting after
// every candidate directory.
func ScanKimiSessions(home string, report func(SessionScanProgress[LocatedKimiSession])) error {
	panic("unported: mj-import/src/import/kimi.rs scan_kimi_sessions")
}

func kimiIndexedCandidates(home string, sessions string) ([]kimiScanCandidate, error) {
	panic("unported: mj-import/src/import/kimi.rs kimi_indexed_candidates")
}

// kimiSessionSummary is what one Kimi session's `state.json` says about it.
type kimiSessionSummary struct {
	// Archived is the listing's verdict: the user archived this session in
	// Kimi. Importing a session the user named by id ignores this.
	Archived bool
	Title    string
	// Cwd is nil when neither `state.json` nor Kimi's own index records an
	// absolute working directory.
	Cwd *string
}

// kimiStateListingMetadata is the listing's view of one session directory:
// its title, cwd ("" when none) and archived flag; ok is false when its
// `state.json` cannot be read or parsed, so one damaged session does not
// fail the scan.
func kimiStateListingMetadata(sessionPath string, indexedWorkDir string) (title string, cwd string, archived bool, ok bool) {
	summary, err := kimiSessionSummaryFor(sessionPath, &indexedWorkDir)
	if err != nil {
		return "", "", false, false
	}
	if summary.Cwd != nil {
		cwd = *summary.Cwd
	}
	return summary.Title, cwd, summary.Archived, true
}

// kimiSessionSummaryFor is Rust's `kimi_session_summary` (renamed: the type
// kimiSessionSummary keeps its name). indexedWorkDir is nil when Kimi's
// index records none.
func kimiSessionSummaryFor(sessionPath string, indexedWorkDir *string) (kimiSessionSummary, error) {
	panic("unported: mj-import/src/import/kimi.rs kimi_session_summary")
}

func kimiSessionModifiedAt(sessionPath string, metadata fs.FileInfo) time.Time {
	panic("unported: mj-import/src/import/kimi.rs kimi_session_modified_at")
}

func selectJSONLSession(
	candidates []LocatedCodexSession,
	selection CodexSessionSelection,
	harness string,
) (LocatedCodexSession, error) {
	panic("unported: mj-import/src/import/kimi.rs select_jsonl_session")
}
