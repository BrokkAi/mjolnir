// Port of mj-import/src/import/grok.rs.

package mjimport

import (
	"io/fs"
	"os"
	"path/filepath"
	"time"
)

// grokCwdDirectory is how a refusal names the directory a Grok Build
// session sits in. Grok Build names that directory after the working
// directory the session ran in, and the listing and the by-id lookup both
// refuse it in these words.
const grokCwdDirectory = "working directory"

// LocateGrokSession locates a Grok Build session directory. Its name is the
// session UUID, which is the native identifier `session/load` takes.
func LocateGrokSession(home string, selection GrokSessionSelection) (LocatedGrokSession, error) {
	panic("unported: mj-import/src/import/grok.rs locate_grok_session")
}

// locateNamedGrokSession finds the session directory a named id points at,
// for the ids the listing leaves out. The listing skips whatever it cannot
// make a row from, including a session whose working directory it cannot
// recover. Naming a session must say why it was skipped rather than report
// it as missing.
//
// A directory reached through a symlink is still refused, because the
// archive step stores only files that sit directly inside the harness home.
func locateNamedGrokSession(home string, nativeSessionID string) (LocatedGrokSession, error) {
	panic("unported: mj-import/src/import/grok.rs locate_named_grok_session")
}

// ListGrokSessions lists native Grok Build sessions newest first.
func ListGrokSessions(home string) ([]LocatedGrokSession, error) {
	sessions := []LocatedGrokSession{}
	err := ScanGrokSessions(home, func(progress SessionScanProgress[LocatedGrokSession]) {
		if progress.Session != nil {
			sessions = append(sessions, *progress.Session)
		}
	})
	if err != nil {
		return nil, err
	}
	return sessions, nil
}

// ScanGrokSessions scans native Grok Build sessions newest first, reporting
// after every candidate directory.
func ScanGrokSessions(home string, report func(SessionScanProgress[LocatedGrokSession])) error {
	panic("unported: mj-import/src/import/grok.rs scan_grok_sessions")
}

// grokCandidates walks `sessions/<encoded-cwd>/<session-uuid>`. The sessions
// root also holds the shared search index and lock files, which are not
// sessions.
//
// A working directory the import could not archive is left out, so the
// listing offers only sessions that can actually be imported and the by-id
// lookup is left to report why one is missing from it.
func grokCandidates(sessions string) ([]kimiScanCandidate, error) {
	panic("unported: mj-import/src/import/grok.rs grok_candidates")
}

// grokDecodeCwdDirname decodes Grok Build's session-directory name: the
// URL-encoded working directory, or a `{slug}-{hash}` form for a long path
// that keeps the real value in a `.cwd` file. Mirrors
// `decode_cwd_from_dirname` in grok-build.
func grokDecodeCwdDirname(directory string) (string, bool) {
	panic("unported: mj-import/src/import/grok.rs grok_decode_cwd_dirname")
}

// urlDecode is percent-decoding for the session-directory name. false when
// the text is not valid percent-encoded UTF-8, which means it is not a cwd
// key.
func urlDecode(value string) (string, bool) {
	panic("unported: mj-import/src/import/grok.rs url_decode")
}

// grokListingMetadata is the listing title and cwd from `summary.json`, each
// nil when absent. Never fails the whole scan: an unreadable session is
// listed with what could be recovered.
func grokListingMetadata(sessionPath string) (title *string, cwd *string) {
	panic("unported: mj-import/src/import/grok.rs grok_listing_metadata")
}

func grokSessionModifiedAt(sessionPath string, metadata fs.FileInfo) time.Time {
	modifiedAt := metadata.ModTime()
	for _, name := range []string{chatHistory, "events.jsonl", "summary.json"} {
		if info, err := os.Stat(filepath.Join(sessionPath, name)); err == nil && info.ModTime().After(modifiedAt) {
			modifiedAt = info.ModTime()
		}
	}
	return modifiedAt
}
