// Port of mj-import/src/import/adapter_transcript.rs.
//
// The Rust module parses native sessions with the SessionWiki adapters of
// the external crate brokk-sessionwiki (`sessionwiki::adapters::{ClaudeCode,
// Codex, GrokBuild, KimiCode, Muse}` and `sessionwiki::model::{Session,
// Role}`). No Go equivalent of that crate exists yet. sessionwikiAdapter,
// sessionwikiSession and sessionwikiMessage declare the part of its API this
// module uses, so the importer is written against it; importAdapter returns
// an implementation once the adapters exist in Go.

package mjimport

import (
	"errors"
	"fmt"
	"path/filepath"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sessionwikiAdapter is the part of `sessionwiki::adapters::Adapter` the
// importer uses.
type sessionwikiAdapter interface {
	// NativeID is the literal harness identity, when it can be resolved from
	// the key or source.
	NativeID(key string) (string, bool)
	// Parse parses one session file.
	Parse(path string) (sessionwikiSession, error)
}

// sessionwikiRole is `sessionwiki::model::Role`.
type sessionwikiRole int

const (
	sessionwikiRoleUser sessionwikiRole = iota
	sessionwikiRoleAssistant
	sessionwikiRoleTool
)

// sessionwikiMessage is the part of `sessionwiki::model::Message` the
// projection reads.
type sessionwikiMessage struct {
	Role sessionwikiRole
	Text string
	// Ts is nil when the message has no timestamp.
	Ts *time.Time
	// IsThought is true for assistant reasoning summaries.
	IsThought bool
}

// sessionwikiSession is the part of `sessionwiki::model::Session` the
// projection reads.
type sessionwikiSession struct {
	// RawCwd is the literal native working directory; nil when absent.
	RawCwd *string
	// OperationalEditedPaths are literal paths backed by the adapter's native
	// write evidence.
	OperationalEditedPaths []string
	// CompactedPrefix: the source begins with compacted history before its
	// first real prompt.
	CompactedPrefix bool
	// Ended is nil when unknown.
	Ended    *time.Time
	Messages []sessionwikiMessage
}

// importAdapter is the SessionWiki adapter that reads a harness's native
// sessions; false for the harnesses Mjolnir cannot import.
func importAdapter(harness core.HarnessKind) (sessionwikiAdapter, bool) {
	panic("unported: mj-import/src/import/adapter_transcript.rs import_adapter")
}

func ReadClaudeTranscript(path string) (ClaudeTranscript, error) {
	return readProjection(core.HarnessKindClaude, path, path)
}

func ReadCodexTranscript(path string) (CodexTranscript, error) {
	summary, err := codexSessionSummaryFor(path)
	if err != nil {
		return ClaudeTranscript{}, err
	}
	if issue, ok := summary.HistoryMode.ImportIssue(); ok {
		return ClaudeTranscript{}, errors.New(issue)
	}
	return readProjection(core.HarnessKindCodex, path, path)
}

func ReadKimiTranscript(path string) (KimiTranscript, error) {
	source := filepath.Join(path, "agents", "main", "wire.jsonl")
	if !importIsFile(source) {
		source = filepath.Join(path, "wire.jsonl")
	}
	return readProjection(core.HarnessKindKimi, path, source)
}

func ReadGrokTranscript(path string) (GrokTranscript, error) {
	source := filepath.Join(path, chatHistory)
	if !importIsFile(source) {
		source = filepath.Join(path, "history.jsonl")
	}
	return readProjection(core.HarnessKindGrok, path, source)
}

func readMuseTranscript(path string, originalKey string) (ClaudeTranscript, error) {
	cwd, err := museValidateTranscript(path, originalKey)
	if err != nil {
		return ClaudeTranscript{}, err
	}
	transcript, err := readProjection(core.HarnessKindMuse, path, path)
	if err != nil {
		return ClaudeTranscript{}, err
	}
	if transcript.Cwd != cwd {
		return ClaudeTranscript{}, errors.New("Muse session workspace changed while it was being read")
	}
	return transcript, nil
}

func readProjection(harness core.HarnessKind, key string, source string) (ClaudeTranscript, error) {
	adapter, ok := importAdapter(harness)
	if !ok {
		return ClaudeTranscript{}, errors.New("native harness does not support import")
	}
	session, err := adapter.Parse(key)
	if err != nil {
		return ClaudeTranscript{}, fmt.Errorf("read %s session %s: %w", harness.DisplayName(), key, err)
	}
	return projectSession(session, source)
}

// projectSession projects a parsed native session into the canonical
// transcript. source supplies a clock for older streams with no timed
// dialogue.
func projectSession(session sessionwikiSession, source string) (ClaudeTranscript, error) {
	panic("unported: mj-import/src/import/adapter_transcript.rs project_session")
}

// materialize is the imported transcript as a materialized session with id
// sessionID.
func (t *ClaudeTranscript) materialize(sessionID string) core.MaterializedSession {
	panic("unported: mj-import/src/import/adapter_transcript.rs ClaudeTranscript::materialize")
}
