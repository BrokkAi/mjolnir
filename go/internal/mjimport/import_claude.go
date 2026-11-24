// Port of mj-import/src/import/claude.rs.

package mjimport

import (
	"fmt"
	"strings"
	"unicode"
)

// LocateClaudeSession locates one native Claude rollout. Latest compares
// modified time across every immediate project directory, exactly as
// Claude's layout requires.
func LocateClaudeSession(home string, selection ClaudeSessionSelection) (LocatedClaudeSession, error) {
	panic("unported: mj-import/src/import/claude.rs locate_claude_session")
}

// unlistedClaudeSessions is what looking a named Claude session up by path
// found: the transcripts that can be imported, and why any file that sits at
// the named path cannot be.
type unlistedClaudeSessions struct {
	Matches  []LocatedClaudeSession
	Rejected []string
}

// locateUnlistedClaudeSessions finds the transcript a named session id
// points at, for the ids the listing does not show. The listing is a resume
// picker: it stops at Claude's own display limit and hides sidechains, team
// sessions, daemon workers and non-interactive entrypoints. None of that
// applies once the user names a session, so this lookup reads
// `projects/<project>/<id>.jsonl` whatever the picker would have made of it.
//
// A transcript reached through a symlink is still refused, because the
// archive step only stores regular files inside the harness home and would
// otherwise copy content from outside it. That refusal is reported, so the
// caller can say why the named path was rejected instead of "not found".
func locateUnlistedClaudeSessions(home string, nativeSessionID string) (unlistedClaudeSessions, error) {
	panic("unported: mj-import/src/import/claude.rs locate_unlisted_claude_sessions")
}

// ListClaudeSessions lists native Claude sessions newest first.
func ListClaudeSessions(home string) ([]LocatedClaudeSession, error) {
	sessions := []LocatedClaudeSession{}
	err := ScanClaudeSessions(home, NewNativeScanCache(), func(progress SessionScanProgress[LocatedClaudeSession]) {
		if progress.Session != nil {
			sessions = append(sessions, *progress.Session)
		}
	})
	if err != nil {
		return nil, err
	}
	return sessions, nil
}

// ScanClaudeSessions scans native Claude sessions newest first, reporting
// after every candidate file.
func ScanClaudeSessions(
	home string,
	cache *NativeScanCache,
	report func(SessionScanProgress[LocatedClaudeSession]),
) error {
	panic("unported: mj-import/src/import/claude.rs scan_claude_sessions")
}

// claudeLineMayMatter reports whether one transcript line can still change
// what claudeNativeMetadata returns. Every key that function reads has a
// byte substring here, so a line that matches none of them parses to nothing
// the function would use.
func claudeLineMayMatter(line string, needsCwd bool, needsGitBranch bool, needsEntrypoint bool) bool {
	return (needsCwd && strings.Contains(line, `"cwd"`)) ||
		(needsGitBranch && strings.Contains(line, `"gitBranch"`)) ||
		(needsEntrypoint && strings.Contains(line, `"entrypoint"`)) ||
		strings.Contains(line, `"customTitle"`) ||
		strings.Contains(line, `"aiTitle"`) ||
		strings.Contains(line, `"agentName"`) ||
		// Filter markers: a sidechain, a team session, a daemon worker, or a
		// `/loop` command record.
		jsonFieldIsTrue(line, "isSidechain") ||
		strings.Contains(line, `"teamName"`) ||
		strings.Contains(line, "daemon-worker") ||
		strings.Contains(line, "<command-name>/loop</command-name>")
}

// jsonFieldIsTrue reports whether `"<field>"` is followed by `true`
// somewhere in this line, allowing for whitespace around the colon.
func jsonFieldIsTrue(line string, field string) bool {
	rest := line
	for {
		index := strings.Index(rest, field)
		if index < 0 {
			return false
		}
		rest = rest[index+len(field):]
		if after, ok := strings.CutPrefix(strings.TrimLeftFunc(rest, unicode.IsSpace), `"`); ok {
			if after, ok := strings.CutPrefix(strings.TrimLeftFunc(after, unicode.IsSpace), ":"); ok &&
				strings.HasPrefix(strings.TrimLeftFunc(after, unicode.IsSpace), "true") {
				return true
			}
		}
	}
}

// claudeNativeSummary is what one Claude transcript's own records say about
// it.
type claudeNativeSummary struct {
	// Subagent is positive child ownership, independently of resume
	// eligibility.
	Subagent bool
	// Filtered is the resume picker's verdict: a sidechain, a team session, a
	// daemon worker, a `/loop` record, or a non-interactive entrypoint.
	// Importing a session the user named by id ignores this.
	Filtered bool
	Title    string
	// Cwd is nil when no record in the file carries a `cwd`.
	Cwd       *string
	GitBranch string
}

// claudeListing is the (title, cwd, git branch) a Claude transcript lists
// with: Rust's `(String, PathBuf, String)`.
type claudeListing struct {
	title     string
	cwd       string
	gitBranch string
}

// claudeNativeMetadata is the picker's view of one transcript: nil when the
// file is filtered out of the resume list, and an error when it has no `cwd`
// to import from.
func claudeNativeMetadata(path string) (*claudeListing, error) {
	summary, err := claudeNativeSummaryFor(path)
	if err != nil {
		return nil, err
	}
	if summary.Filtered {
		return nil, nil
	}
	if summary.Cwd == nil {
		return nil, fmt.Errorf("Claude session %s has no cwd", path)
	}
	return &claudeListing{title: summary.Title, cwd: *summary.Cwd, gitBranch: summary.GitBranch}, nil
}

// claudeNativeSummaryFor is Rust's `claude_native_summary` (renamed: the
// type claudeNativeSummary keeps its name).
func claudeNativeSummaryFor(path string) (claudeNativeSummary, error) {
	panic("unported: mj-import/src/import/claude.rs claude_native_summary")
}

func gitBranchOrHead(cwd string) string {
	if cwd == "" {
		return "HEAD"
	}
	branch, ok, err := gitOptionalText(cwd, "branch", "--show-current")
	if err != nil || !ok || branch == "" {
		return "HEAD"
	}
	return branch
}

// DirectorySize is the total size of the regular files under path, not
// following symlinks.
func DirectorySize(path string) (uint64, error) {
	panic("unported: mj-import/src/import/claude.rs directory_size")
}

func claudeCandidates(home string) ([]fileScanCandidate, error) {
	panic("unported: mj-import/src/import/claude.rs claude_candidates")
}
