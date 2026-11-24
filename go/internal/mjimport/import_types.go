// Port of mj-import/src/import/types.rs.

package mjimport

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ClaudeSessionSelection is the native session to import. One of
// ClaudeSessionSelectionNativeSessionID or ClaudeSessionSelectionLatest.
type ClaudeSessionSelection interface {
	isClaudeSessionSelection()
}

// ClaudeSessionSelectionNativeSessionID names one native session by its id.
type ClaudeSessionSelectionNativeSessionID string

// ClaudeSessionSelectionLatest selects the newest native session.
type ClaudeSessionSelectionLatest struct{}

func (ClaudeSessionSelectionNativeSessionID) isClaudeSessionSelection() {}
func (ClaudeSessionSelectionLatest) isClaudeSessionSelection()          {}

type CodexSessionSelection = ClaudeSessionSelection
type KimiSessionSelection = ClaudeSessionSelection
type GrokSessionSelection = ClaudeSessionSelection

type LocatedClaudeSession struct {
	NativeSessionID string
	JSONLPath       string
	ModifiedAt      time.Time
	Title           string
	Cwd             string
	GitBranch       string
	SizeBytes       uint64
}

type CodexHistoryMode int

const (
	CodexHistoryModeLegacy CodexHistoryMode = iota
	CodexHistoryModePaginated
)

// chatHistory is Grok Build's conversation of record inside a session
// directory.
const chatHistory = "chat_history.jsonl"

// CodexLegacyImportIssue is shown by the dashboard's import tab and by
// `mj import`, so it names no surface of its own (launch finding R2-9).
const CodexLegacyImportIssue = "Legacy Codex history cannot be imported. Run codex migrate-rollouts --apply, then try the import again."

// ImportIssue is why a rollout in this history mode cannot be imported;
// false when it can be.
func (m CodexHistoryMode) ImportIssue() (string, bool) {
	switch m {
	case CodexHistoryModeLegacy:
		return CodexLegacyImportIssue, true
	default:
		return "", false
	}
}

type LocatedCodexSession struct {
	NativeSessionID string
	JSONLPath       string
	ModifiedAt      time.Time
	Title           string
	Cwd             string
	GitBranch       string
	SizeBytes       uint64
	HistoryMode     CodexHistoryMode
	// NativelyArchived is archived inside Codex itself. Hel mirrors that one
	// way: the row is hidden by default and never written back to Codex.
	NativelyArchived bool
}

type LocatedKimiSession struct {
	NativeSessionID string
	SessionPath     string
	ModifiedAt      time.Time
	Title           string
	Cwd             string
	GitBranch       string
	SizeBytes       uint64
}

// LocatedGrokSession is a Grok Build session; Grok Build keeps one directory
// per session, like Kimi Code.
type LocatedGrokSession = LocatedKimiSession

type SessionScanProgress[T any] struct {
	Scanned int
	Total   int
	// Session is nil when this report carries no session.
	Session *T
}

type fileScanCandidate struct {
	path       string
	modifiedAt time.Time
	sizeBytes  uint64
}

type kimiScanCandidate struct {
	nativeSessionID string
	sessionPath     string
	modifiedAt      time.Time
	title           string
	cwd             string
}

type codexSessionMetadata struct {
	id          string
	cwd         string
	gitBranch   string
	historyMode CodexHistoryMode
}

type ClaudeTranscript struct {
	Cwd string
	// EditedPaths are files reliably reported as edited by the native
	// harness.
	EditedPaths []string
	items       []*core.TranscriptItem
	// lastActivityAtMs is nil for Rust's None.
	lastActivityAtMs *int64
}

type CodexTranscript = ClaudeTranscript
type KimiTranscript = ClaudeTranscript
type GrokTranscript = ClaudeTranscript

// BundleResolution is the bundle an import uses. One of
// BundleResolutionExisting or BundleResolutionSynthesized.
type BundleResolution interface {
	isBundleResolution()
}

// BundleResolutionExisting is a configured bundle, by id.
type BundleResolutionExisting string

// BundleResolutionSynthesized is a new bundle. The caller must ask the user
// before adding this to their config.
type BundleResolutionSynthesized struct {
	ID     string
	Bundle core.ProjectBundle
}

func (BundleResolutionExisting) isBundleResolution()    {}
func (BundleResolutionSynthesized) isBundleResolution() {}

type SessionEditTargets struct {
	GitRoots []string
	// ScratchGitRoots are Git roots under a temporary directory. They are
	// throwaway workspaces rather than project repositories, so the import
	// omits them.
	ScratchGitRoots []string
	NonGitDirs      []string
}

type ImportSafetyIssues struct {
	// DirtyGitRoots are (root, description) pairs.
	DirtyGitRoots     [][2]string
	OmittedNonGitDirs []string
	ScratchGitRoots   []string
	HasUntrackedFiles bool
}
