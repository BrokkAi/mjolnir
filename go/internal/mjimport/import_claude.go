// Port of mj-import/src/import/claude.rs.

package mjimport

import (
	"errors"
	"fmt"
	"io/fs"
	"math"
	"os"
	"path/filepath"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// LocateClaudeSession locates one native Claude rollout. Latest compares
// modified time across every immediate project directory, exactly as
// Claude's layout requires.
func LocateClaudeSession(home string, selection ClaudeSessionSelection) (LocatedClaudeSession, error) {
	candidates, err := ListClaudeSessions(home)
	if err != nil {
		return LocatedClaudeSession{}, err
	}
	projects := filepath.Join(home, "projects")
	switch selection := selection.(type) {
	case ClaudeSessionSelectionNativeSessionID:
		nativeSessionID := string(selection)
		if err := core.ValidateID("Claude session", nativeSessionID); err != nil {
			return LocatedClaudeSession{}, err
		}
		matches := []LocatedClaudeSession{}
		for _, candidate := range candidates {
			// A listed row can carry an empty cwd: ScanClaudeSessions keeps a
			// transcript it failed to parse in the picker rather than hiding
			// it. Such a row cannot be imported, so the by-id lookup re-reads
			// the file and reports the parse failure or the missing cwd by
			// name.
			if candidate.NativeSessionID == nativeSessionID && candidate.Cwd != "" {
				matches = append(matches, candidate)
			}
		}
		rejected := []string{}
		if len(matches) == 0 {
			unlisted, err := locateUnlistedClaudeSessions(home, nativeSessionID)
			if err != nil {
				return LocatedClaudeSession{}, err
			}
			matches = unlisted.Matches
			rejected = unlisted.Rejected
		}
		switch {
		case len(matches) == 0 && len(rejected) > 0:
			return LocatedClaudeSession{}, claudeStore.CannotImport(nativeSessionID, rejected)
		case len(matches) == 0:
			return LocatedClaudeSession{}, fmt.Errorf("Claude session %s was not found under %s",
				core.RustDebugString(nativeSessionID), projects)
		case len(matches) == 1:
			return matches[0], nil
		default:
			return LocatedClaudeSession{}, fmt.Errorf("Claude session %s occurs in multiple project directories",
				core.RustDebugString(nativeSessionID))
		}
	case ClaudeSessionSelectionLatest:
		if len(candidates) == 0 {
			return LocatedClaudeSession{}, errors.New("no Claude session JSONL files were found")
		}
		return candidates[0], nil
	}
	panic(fmt.Sprintf("unknown session selection %T", selection))
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
	projects := filepath.Join(home, "projects")
	result := unlistedClaudeSessions{Matches: []LocatedClaudeSession{}, Rejected: []string{}}
	entries, err := os.ReadDir(projects)
	if err != nil {
		return unlistedClaudeSessions{}, importIOError(err)
	}
	for _, project := range entries {
		projectPath := filepath.Join(projects, project.Name())
		path := filepath.Join(projectPath, nativeSessionID+".jsonl")
		switch entry := claudeStore.Container(projectPath, "project directory").(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			// A project directory the archive cannot use is worth reporting
			// only when the named session is visible inside it.
			if importExists(path) {
				result.Rejected = append(result.Rejected, string(entry))
			}
			continue
		}
		var metadata fs.FileInfo
		switch entry := claudeStore.File(path).(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			result.Rejected = append(result.Rejected, string(entry))
			continue
		case namedEntryImportable:
			metadata = entry.metadata
		}
		summary, err := claudeNativeSummaryFor(path)
		if err != nil {
			return unlistedClaudeSessions{}, err
		}
		// A transcript that never records a cwd cannot be imported: the
		// archive is collected relative to the directory the session ran in.
		if summary.Cwd == nil {
			result.Rejected = append(result.Rejected, claudeStore.NoCwd(path))
			continue
		}
		result.Matches = append(result.Matches, LocatedClaudeSession{
			NativeSessionID: nativeSessionID,
			JSONLPath:       path,
			ModifiedAt:      metadata.ModTime(),
			Title:           summary.Title,
			Cwd:             *summary.Cwd,
			GitBranch:       summary.GitBranch,
			SizeBytes:       uint64(metadata.Size()),
		})
	}
	return result, nil
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
	candidates, err := claudeCandidates(home)
	if err != nil {
		return err
	}
	total := len(candidates)
	report(SessionScanProgress[LocatedClaudeSession]{Scanned: 0, Total: total})
	visible := 0
	for index, candidate := range candidates {
		if visible == claudeAllProjectsLimit {
			report(SessionScanProgress[LocatedClaudeSession]{Scanned: index + 1, Total: total})
			continue
		}
		name, _ := importPathFileName(candidate.path)
		sessionID, ok := strings.CutSuffix(name, ".jsonl")
		if !ok {
			panic("Claude candidates were validated during enumeration")
		}
		metadata, err := cache.claudeMetadata(candidate.path, candidate.modifiedAt, candidate.sizeBytes, func() (*claudeListing, error) {
			return claudeNativeMetadata(candidate.path)
		})
		if err != nil {
			// A transcript Mjolnir cannot parse still belongs in the picker:
			// dropping it is how a named session became invisible in the
			// first place. The row carries an empty cwd, which the by-id
			// lookup refuses, so it re-reads the file and reports the parse
			// failure rather than importing a session with no directory.
			metadata = &claudeListing{title: sessionID, cwd: "", gitBranch: "HEAD"}
		} else if metadata == nil {
			report(SessionScanProgress[LocatedClaudeSession]{Scanned: index + 1, Total: total})
			continue
		}
		visible++
		report(SessionScanProgress[LocatedClaudeSession]{
			Scanned: index + 1,
			Total:   total,
			Session: &LocatedClaudeSession{
				NativeSessionID: sessionID,
				JSONLPath:       candidate.path,
				ModifiedAt:      candidate.modifiedAt,
				Title:           metadata.title,
				Cwd:             metadata.cwd,
				GitBranch:       metadata.gitBranch,
				SizeBytes:       candidate.sizeBytes,
			},
		})
	}
	return nil
}

// claudeAllProjectsLimit is how many sessions Claude's own all-projects
// resume picker lists.
const claudeAllProjectsLimit = 50

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
	var customTitle, agentName, aiTitle, cwd, gitBranch, entrypoint *string
	filtered := false
	subagent := false
	file, err := os.Open(path)
	if err != nil {
		return claudeNativeSummary{}, importIOError(err)
	}
	defer file.Close()
	lines := newImportLineReader(file)
	for {
		line, ok, err := lines.next()
		if err != nil {
			return claudeNativeSummary{}, err
		}
		if !ok {
			break
		}
		// Parsing every record of every transcript is what made the import
		// scan slow: a large session is megabytes of JSON, and only a handful
		// of records can change the answer. A line that cannot contain any of
		// the keys read below is skipped without being parsed. The position
		// keys drop out of the test as soon as their value is known, which is
		// usually on the first line.
		if !claudeLineMayMatter(line, cwd == nil, gitBranch == nil, entrypoint == nil) {
			continue
		}
		record, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			return claudeNativeSummary{}, err
		}
		sidechain, _ := importJSONBool(importJSONGet(record, "isSidechain"))
		_, team := importNonblank(importJSONGet(record, "teamName"))
		child := sidechain || team
		subagent = subagent || child
		if kind, ok := importJSONStr(importJSONGet(record, "sessionKind")); child || (ok && kind == "daemon-worker") {
			filtered = true
		}
		if entrypoint == nil {
			if value, ok := importNonblank(importJSONGet(record, "entrypoint")); ok {
				entrypoint = &value
			}
		}
		if cwd == nil {
			if value, ok := importNonblank(importJSONGet(record, "cwd")); ok {
				cwd = &value
			}
		}
		if gitBranch == nil {
			if value, ok := importNonblank(importJSONGet(record, "gitBranch")); ok {
				gitBranch = &value
			}
		}
		recordType, _ := importJSONStr(importJSONGet(record, "type"))
		switch recordType {
		case "custom-title":
			if nativeTitle, ok := importNonblank(importJSONGet(record, "customTitle")); ok {
				customTitle = claudeNormalizedTitle(nativeTitle)
			}
		case "ai-title":
			if nativeTitle, ok := importNonblank(importJSONGet(record, "aiTitle")); ok {
				aiTitle = claudeNormalizedTitle(nativeTitle)
			}
		case "agent-name":
			agentName = nil
			if nativeTitle, ok := importNonblank(importJSONGet(record, "agentName")); ok {
				agentName = claudeNormalizedTitle(nativeTitle)
			}
		case "user":
			content, ok := importJSONStr(importJSONPointer(record, "message", "content"))
			if ok && strings.Contains(content, "<command-name>/loop</command-name>") {
				filtered = true
			}
		}
	}
	// Claude's native resume picker is for interactive CLI conversations. In
	// particular, its print/SDK entrypoints include the tiny rollouts created
	// by `claude -p /usage`, which must not displace real sessions there.
	title := "Untitled session"
	for _, candidate := range []*string{customTitle, agentName, aiTitle} {
		if candidate != nil {
			title = *candidate
			break
		}
	}
	branch := "HEAD"
	if gitBranch != nil {
		branch = *gitBranch
	}
	return claudeNativeSummary{
		Subagent:  subagent,
		Filtered:  filtered || (entrypoint != nil && *entrypoint != "cli"),
		Title:     title,
		Cwd:       cwd,
		GitBranch: branch,
	}, nil
}

// claudeNormalizedTitle is NormalizeSessionTitle as an optional value.
func claudeNormalizedTitle(title string) *string {
	normalized, ok := core.NormalizeSessionTitle(title)
	if !ok {
		return nil
	}
	return &normalized
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
	entries, err := os.ReadDir(path)
	if err != nil {
		return 0, importIOError(err)
	}
	var size uint64
	for _, entry := range entries {
		entryPath := filepath.Join(path, entry.Name())
		metadata, err := os.Lstat(entryPath)
		if err != nil {
			return 0, importIOError(err)
		}
		if metadata.Mode().IsRegular() {
			size = importSaturatingAdd(size, uint64(metadata.Size()))
		} else if metadata.IsDir() {
			nested, err := DirectorySize(entryPath)
			if err != nil {
				return 0, err
			}
			size = importSaturatingAdd(size, nested)
		}
	}
	return size, nil
}

// importSaturatingAdd is Rust's `u64::saturating_add`.
func importSaturatingAdd(a, b uint64) uint64 {
	if a > math.MaxUint64-b {
		return math.MaxUint64
	}
	return a + b
}

func claudeCandidates(home string) ([]fileScanCandidate, error) {
	projects := filepath.Join(home, "projects")
	// Profiles used only by workers may never have created a local history.
	entries, err := os.ReadDir(projects)
	if err != nil {
		if importIsNotFound(err) {
			return []fileScanCandidate{}, nil
		}
		return nil, fmt.Errorf("read Claude projects directory %s: %w", projects, importIOError(err))
	}
	candidates := []fileScanCandidate{}
	for _, project := range entries {
		projectPath := filepath.Join(projects, project.Name())
		projectMetadata, err := os.Lstat(projectPath)
		if err != nil {
			return nil, importIOError(err)
		}
		if projectMetadata.Mode()&fs.ModeSymlink != 0 || !projectMetadata.IsDir() {
			continue
		}
		sessions, err := os.ReadDir(projectPath)
		if err != nil {
			return nil, importIOError(err)
		}
		for _, entry := range sessions {
			path := filepath.Join(projectPath, entry.Name())
			metadata, err := os.Lstat(path)
			if err != nil {
				return nil, importIOError(err)
			}
			if !metadata.Mode().IsRegular() {
				continue
			}
			name := entry.Name()
			if !utf8.ValidString(name) {
				continue
			}
			sessionID, ok := strings.CutSuffix(name, ".jsonl")
			if !ok || sessionID == "" {
				continue
			}
			candidates = append(candidates, fileScanCandidate{
				path:       path,
				modifiedAt: metadata.ModTime(),
				sizeBytes:  uint64(metadata.Size()),
			})
		}
	}
	sortFileCandidatesNewestFirst(candidates)
	return candidates, nil
}
