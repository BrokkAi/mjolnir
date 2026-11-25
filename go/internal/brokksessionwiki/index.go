// Port of brokk-sessionwiki 0.34.28 src/index.rs.

package brokksessionwiki

import (
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// SchemaVersion versions the disposable cache (`user_version`): a mismatch
// drops and rebuilds the derived tables instead of migrating. The durable
// tables (summaries, tags, notes, archive) are versioned separately and
// survive every upgrade.
const SchemaVersion int64 = 9

// ToolOutputMode is whether full tool results are retained in the index and
// archive.
type ToolOutputMode string

const (
	// ToolOutputModeFull retains bounded, redacted tool output separately
	// from searchable text.
	ToolOutputModeFull ToolOutputMode = "full"
	// ToolOutputModeSummary keeps compact tool summary lines. Failed calls
	// include the first output line in their summary.
	ToolOutputModeSummary ToolOutputMode = "summary"
)

// AsStr is the mode's stored name.
func (m ToolOutputMode) AsStr() string { return string(m) }

// DBPath is the index path, creating its directory (and migrating a legacy
// one) when needed: $SESSIONWIKI_DATA/index.db, or index.db in the
// platform data directory's `sessionwiki`.
func DBPath() (string, error) {
	panic("unported: brokk-sessionwiki src/index.rs db_path")
}

// ExistingDBPath is the index path if the file already exists, without the
// directory creation or legacy migration DBPath performs.
func ExistingDBPath() (string, bool) {
	panic("unported: brokk-sessionwiki src/index.rs existing_db_path")
}

// Open opens the index for writing, creating or rebuilding its cache as
// needed.
func Open() (*store.Connection, error) {
	panic("unported: brokk-sessionwiki src/index.rs open")
}

// OpenReadonly opens an existing index read-only; it never creates the
// schema, migrates, or writes durable data.
func OpenReadonly() (*store.Connection, error) {
	panic("unported: brokk-sessionwiki src/index.rs open_readonly")
}

// ReadToolOutputMode is Rust's `tool_output_mode` (renamed because Go cannot
// give the function and the ToolOutputMode type one name): the index's
// retention mode, `summary` when the index has no explicit choice.
func ReadToolOutputMode(conn store.Queryer) (ToolOutputMode, error) {
	panic("unported: brokk-sessionwiki src/index.rs tool_output_mode")
}

// SetToolOutputMode changes tool-output retention for this index; false when
// it already had that mode.
func SetToolOutputMode(conn *store.Connection, mode ToolOutputMode) (bool, error) {
	panic("unported: brokk-sessionwiki src/index.rs set_tool_output_mode")
}

// SyncProgress is one progress report of a sync pass.
type SyncProgress struct {
	Tool     string
	Adapter  int
	Adapters int
	Done     int
	// Total is nil while the adapter's session count is unknown.
	Total *int
}

// SyncWith indexes the sessions of an explicit adapter list. When since is
// non-nil only sessions modified at or after that epoch second are parsed.
func SyncWith(conn *store.Connection, adapters []Adapter, since *int64) error {
	return SyncWithProgress(conn, adapters, since, func(*SyncProgress) {})
}

// SyncWithProgress is SyncWith, reporting progress to onProgress.
func SyncWithProgress(conn *store.Connection, adapters []Adapter, since *int64, onProgress func(*SyncProgress)) error {
	panic("unported: brokk-sessionwiki src/index.rs sync_with_progress")
}

// SessionRow is one indexed session as row queries return it.
type SessionRow struct {
	SessionID string
	Tool      string
	// Path is the native session file path (or store key). It is never
	// serialized as is; JSON carries only its NativeIDOf as `native_id`.
	Path    string
	Project string
	Title   string
	Started *string
	// LastActive is when the session last had a message; nil where it was
	// not read (and then left out of JSON).
	LastActive *string
	MsgCount   int64
	Kind       string
	Preview    *string
	Summary    *string
	// Tags is the comma-joined user tags, serialized as a string array.
	Tags     *string
	Archived bool
	Account  *string
}

// MarshalJSON writes the row as the crate's Serialize does: renamed fields,
// the path reduced to its native id, and tags as an array.
func (r SessionRow) MarshalJSON() ([]byte, error) {
	optional := func(value *string) any {
		if value == nil {
			return nil
		}
		return *value
	}
	var nativeID any
	if id, ok := NativeIDOf(r.Path); ok {
		nativeID = id
	}
	var tags any
	if r.Tags != nil {
		split := []any{}
		for _, tag := range strings.Split(*r.Tags, ",") {
			split = append(split, tag)
		}
		tags = split
	}
	object := jsonvalue.Object{
		{Key: "id", Value: r.SessionID},
		{Key: "tool", Value: r.Tool},
		{Key: "native_id", Value: nativeID},
		{Key: "project", Value: r.Project},
		{Key: "title", Value: r.Title},
		{Key: "started", Value: optional(r.Started)},
	}
	if r.LastActive != nil {
		object = append(object, jsonvalue.Member{Key: "last_active", Value: *r.LastActive})
	}
	object = append(object,
		jsonvalue.Member{Key: "msgs", Value: r.MsgCount},
		jsonvalue.Member{Key: "kind", Value: r.Kind},
		jsonvalue.Member{Key: "preview", Value: optional(r.Preview)},
		jsonvalue.Member{Key: "summary", Value: optional(r.Summary)},
		jsonvalue.Member{Key: "tags", Value: tags},
		jsonvalue.Member{Key: "archived", Value: r.Archived},
		jsonvalue.Member{Key: "account", Value: optional(r.Account)},
	)
	return jsonvalue.Marshal(object)
}

// NativeIDOf is the native session UUID in a session file's name: the first
// canonical 8-4-4-4-12 UUID in the file name, lowercased.
func NativeIDOf(path string) (string, bool) {
	panic("unported: brokk-sessionwiki src/index.rs native_id_of")
}

// Recent is the most recently active indexed sessions.
func Recent(conn store.Queryer, limit int, tool, project, tag *string, includeSubagents bool) ([]SessionRow, error) {
	panic("unported: brokk-sessionwiki src/index.rs recent")
}

// Hit is one session's best matching message for a search.
type Hit struct {
	Row     SessionRow
	Role    string
	Snippet string
}

// SearchWithRoles searches messages and returns each session's best matching
// hit. roles nil matches every role; a non-nil empty list matches nothing.
func SearchWithRoles(conn store.Queryer, query string, limit int, tool, project *string, roles *[]Role) ([]Hit, error) {
	panic("unported: brokk-sessionwiki src/index.rs search_with_roles")
}

// Resolve is the indexed sessions whose short id, or native id, starts with
// idPrefix.
func Resolve(conn store.Queryer, idPrefix string) ([]SessionRow, error) {
	panic("unported: brokk-sessionwiki src/index.rs resolve")
}

// SessionFromIndex reconstructs a session from its indexed copy.
func SessionFromIndex(conn store.Queryer, row *SessionRow) (*Session, error) {
	panic("unported: brokk-sessionwiki src/index.rs session_from_index")
}

// FilesFor is the files a session edited or created, in the order it first
// touched them.
func FilesFor(conn store.Queryer, sessionID string) ([]string, error) {
	panic("unported: brokk-sessionwiki src/index.rs files_for")
}

// SessionFileMatch is one session that touched a file, with the stored path
// the query matched (Rust's `(SessionRow, String)`).
type SessionFileMatch struct {
	Row         SessionRow
	MatchedPath string
}

// SessionsForFile is the sessions that touched a file, newest first.
func SessionsForFile(conn store.Queryer, query string, limit int) ([]SessionFileMatch, error) {
	panic("unported: brokk-sessionwiki src/index.rs sessions_for_file")
}

// SessionsTouching is SessionsForFile with the time window and project blame
// attribution needs.
func SessionsTouching(conn store.Queryer, query string) ([]TouchingSession, error) {
	panic("unported: brokk-sessionwiki src/index.rs sessions_touching")
}
