// Port of mj-import/src/import/codex.rs.

package mjimport

import (
	"bufio"
	"database/sql"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/klauspost/compress/zstd"
	_ "modernc.org/sqlite"

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
	listed, err := ListCodexSessions(home)
	if err != nil {
		return LocatedCodexSession{}, err
	}
	// `--latest` follows Codex's own default view, which hides what the user
	// archived there. Asking for an id by name still finds it.
	if _, latest := selection.(ClaudeSessionSelectionLatest); latest {
		listed = slices.DeleteFunc(listed, func(session LocatedCodexSession) bool {
			return session.NativelyArchived
		})
	}
	if named, ok := selection.(ClaudeSessionSelectionNativeSessionID); ok &&
		!slices.ContainsFunc(listed, func(session LocatedCodexSession) bool {
			return session.NativeSessionID == string(named)
		}) {
		return locateUnindexedCodexSession(home, string(named))
	}
	return selectJSONLSession(listed, selection, "Codex")
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
	if err := core.ValidateID("Codex session", sessionID); err != nil {
		return LocatedCodexSession{}, err
	}
	walk := namedCodexWalk{
		sessionID: sessionID,
		visited:   map[string]struct{}{},
		found:     []fileScanCandidate{},
		rejected:  []string{},
	}
	root := filepath.Join(home, "sessions")
	if importIsDir(root) {
		if err := walk.walk(root, 0, nil); err != nil {
			return LocatedCodexSession{}, err
		}
	}
	titles, err := codexNativeTitles(home)
	if err != nil {
		return LocatedCodexSession{}, err
	}
	matches := []LocatedCodexSession{}
	found := walk.found
	walk.found = nil
	for _, candidate := range found {
		summary, err := codexSessionSummaryFor(candidate.path)
		if err != nil {
			return LocatedCodexSession{}, err
		}
		metadata := summary.Metadata
		if metadata == nil {
			walk.rejected = append(walk.rejected, fmt.Sprintf("%s records no Codex session id", candidate.path))
			continue
		}
		// The rollout at this path belongs to a different thread, so it is
		// not the named session and nothing about it is worth reporting.
		if metadata.id != sessionID {
			continue
		}
		// Codex records the directory a thread ran in once, in its header.
		// The archive is collected relative to that directory.
		if metadata.cwd == "" {
			walk.rejected = append(walk.rejected, codexStore.NoCwd(candidate.path))
			continue
		}
		title, ok := titles[sessionID]
		if !ok {
			title = sessionID
		}
		matches = append(matches, LocatedCodexSession{
			NativelyArchived: false,
			Title:            title,
			NativeSessionID:  metadata.id,
			JSONLPath:        candidate.path,
			ModifiedAt:       candidate.modifiedAt,
			Cwd:              metadata.cwd,
			GitBranch:        metadata.gitBranch,
			SizeBytes:        candidate.sizeBytes,
			HistoryMode:      metadata.historyMode,
		})
	}
	if len(matches) == 0 && len(walk.rejected) > 0 {
		return LocatedCodexSession{}, codexStore.CannotImport(sessionID, walk.rejected)
	}
	return selectJSONLSession(matches, ClaudeSessionSelectionNativeSessionID(sessionID), "Codex")
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
	canonical, err := importCanonicalize(directory)
	if err != nil {
		return nil
	}
	if _, seen := w.visited[canonical]; seen {
		return nil
	}
	w.visited[canonical] = struct{}{}
	entries, err := os.ReadDir(directory)
	if err != nil {
		// A directory the lookup cannot read is one more reason the named
		// session was not found, not a failure of the lookup: whatever else
		// the tree holds is still worth reading.
		w.rejected = append(w.rejected, cannotRead(directory, err))
		return nil
	}
	for _, entry := range entries {
		path := filepath.Join(directory, entry.Name())
		metadata, err := os.Lstat(path)
		if err != nil {
			return importIOError(err)
		}
		// A rollout filename ends in the thread's UUID. Only a path that
		// names this session is worth refusing out loud; a file with no UUID
		// in its name is still read, because its header may name the session
		// anyway.
		rolloutID, hasRolloutID := codexRolloutIDFromPath(path)
		namesSession := hasRolloutID && rolloutID == w.sessionID
		maySession := !hasRolloutID || rolloutID == w.sessionID
		if metadata.Mode()&fs.ModeSymlink != 0 {
			if target, err := os.Stat(path); err == nil && target.IsDir() {
				if depth < codexRolloutDepth {
					via := viaSymlink
					if via == nil {
						linked := path
						via = &linked
					}
					if err := w.walk(path, depth+1, via); err != nil {
						return err
					}
				}
			} else if namesSession {
				if reason, ok := codexStore.File(path).(namedEntryRejected); ok {
					w.rejected = append(w.rejected, string(reason))
				}
			}
			continue
		}
		if metadata.IsDir() {
			if depth < codexRolloutDepth {
				if err := w.walk(path, depth+1, viaSymlink); err != nil {
					return err
				}
			}
			continue
		}
		if extension, ok := importPathExtension(path); !maySession || !ok || extension != "jsonl" {
			continue
		}
		if viaSymlink != nil {
			if namesSession {
				w.rejected = append(w.rejected, codexStore.SymlinkedContainer(*viaSymlink, "rollout directory"))
			}
			continue
		}
		switch entry := codexStore.File(path).(type) {
		case namedEntryAbsent:
			continue
		case namedEntryRejected:
			if namesSession {
				w.rejected = append(w.rejected, string(entry))
			}
		case namedEntryImportable:
			w.found = append(w.found, fileScanCandidate{
				path:       path,
				modifiedAt: entry.metadata.ModTime(),
				sizeBytes:  uint64(entry.metadata.Size()),
			})
		}
	}
	return nil
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
	indexed, ok, err := codexIndexedSessions(home)
	if err != nil {
		return err
	}
	if ok {
		total := len(indexed)
		report(SessionScanProgress[LocatedCodexSession]{Scanned: 0, Total: total})
		for index := range indexed {
			report(SessionScanProgress[LocatedCodexSession]{Scanned: index + 1, Total: total, Session: &indexed[index]})
		}
		return nil
	}

	// Native Codex only indexes threads with a non-empty preview/name. Its
	// history and session-name index provide the same compact set of IDs,
	// avoiding an expensive parse of every exec and subagent rollout.
	titles, err := codexNativeTitles(home)
	if err != nil {
		return err
	}
	candidates := []fileScanCandidate{}
	root := filepath.Join(home, "sessions")
	if importIsDir(root) {
		if err := collectCodexCandidatePaths(root, 0, titles, &candidates); err != nil {
			return err
		}
	}
	sortFileCandidatesNewestFirst(candidates)
	total := len(candidates)
	report(SessionScanProgress[LocatedCodexSession]{Scanned: 0, Total: total})
	for index, candidate := range candidates {
		metadata, err := cache.codexMetadata(candidate.path, candidate.modifiedAt, candidate.sizeBytes, func() (*codexSessionMetadata, error) {
			return codexSessionMetadataFor(candidate.path)
		})
		if err != nil {
			return err
		}
		var session *LocatedCodexSession
		if metadata != nil {
			title, ok := titles[metadata.id]
			if !ok {
				title = metadata.id
			}
			session = &LocatedCodexSession{
				NativelyArchived: false,
				Title:            title,
				NativeSessionID:  metadata.id,
				JSONLPath:        candidate.path,
				ModifiedAt:       candidate.modifiedAt,
				Cwd:              metadata.cwd,
				GitBranch:        metadata.gitBranch,
				SizeBytes:        candidate.sizeBytes,
				HistoryMode:      metadata.historyMode,
			}
		}
		report(SessionScanProgress[LocatedCodexSession]{Scanned: index + 1, Total: total, Session: session})
	}
	return nil
}

// codexIndexedSessions reads Codex's own thread index; false when the home
// has none Mjolnir can read.
func codexIndexedSessions(home string) ([]LocatedCodexSession, bool, error) {
	return codexIndexedSessionsLimited(home, math.MaxInt)
}

func codexIndexedSessionsLimited(home string, limit int) ([]LocatedCodexSession, bool, error) {
	database := filepath.Join(home, "state_5.sqlite")
	if !importIsFile(database) {
		return nil, false, nil
	}
	connection, err := sql.Open("sqlite", importSQLiteReadOnlyURI(database))
	if err != nil {
		return nil, false, err
	}
	defer connection.Close()
	// One connection, as rusqlite's Connection is one.
	connection.SetMaxOpenConns(1)
	if err := connection.Ping(); err != nil {
		return nil, false, err
	}
	historyModeColumn := "'legacy'"
	if probe, err := connection.Prepare("SELECT history_mode FROM threads LIMIT 0"); err == nil {
		probe.Close()
		historyModeColumn = "history_mode"
	}
	// Codex's own archived threads are listed too, flagged rather than
	// filtered: the resume dialog hides them until "show archived" is on, and
	// Hel never writes this database back.
	query := fmt.Sprintf(
		"SELECT id, rollout_path, updated_at, COALESCE(NULLIF(name, ''), NULLIF(title, ''), id), cwd, "+
			"COALESCE(NULLIF(git_branch, ''), 'HEAD'), %s, archived "+
			"FROM threads "+
			"WHERE source IN ('cli', 'vscode') "+
			"AND preview <> '' "+
			"AND rollout_path IS NOT NULL "+
			"ORDER BY updated_at DESC, id DESC LIMIT %d",
		historyModeColumn, int64(limit))
	statement, err := connection.Prepare(query)
	if err != nil {
		return nil, false, nil
	}
	defer statement.Close()
	rows, err := statement.Query()
	if err != nil {
		return nil, false, err
	}
	defer rows.Close()
	sessions := []LocatedCodexSession{}
	for rows.Next() {
		var columns [8]any
		pointers := make([]any, len(columns))
		for index := range columns {
			pointers[index] = &columns[index]
		}
		if err := rows.Scan(pointers...); err != nil {
			return nil, false, err
		}
		sessionID, err := codexColumnText(columns[0], 0)
		if err != nil {
			return nil, false, err
		}
		path, err := codexColumnText(columns[1], 1)
		if err != nil {
			return nil, false, err
		}
		updatedAt, err := codexColumnInteger(columns[2], 2)
		if err != nil {
			return nil, false, err
		}
		title, err := codexColumnText(columns[3], 3)
		if err != nil {
			return nil, false, err
		}
		cwd, err := codexColumnText(columns[4], 4)
		if err != nil {
			return nil, false, err
		}
		gitBranch, err := codexColumnText(columns[5], 5)
		if err != nil {
			return nil, false, err
		}
		historyMode, err := codexColumnText(columns[6], 6)
		if err != nil {
			return nil, false, err
		}
		archived, err := codexColumnInteger(columns[7], 7)
		if err != nil {
			return nil, false, err
		}
		if core.ValidateID("Codex session", sessionID) != nil || updatedAt < 0 {
			continue
		}
		metadata, err := os.Lstat(path)
		if err != nil {
			continue
		}
		if !metadata.Mode().IsRegular() {
			continue
		}
		mode, err := parseCodexHistoryMode(historyMode)
		if err != nil {
			return nil, false, err
		}
		normalized, ok := core.NormalizeSessionTitle(title)
		if !ok {
			normalized = sessionID
		}
		sessions = append(sessions, LocatedCodexSession{
			NativeSessionID:  sessionID,
			JSONLPath:        path,
			ModifiedAt:       time.Unix(updatedAt, 0),
			Title:            normalized,
			Cwd:              cwd,
			GitBranch:        gitBranch,
			SizeBytes:        uint64(metadata.Size()),
			HistoryMode:      mode,
			NativelyArchived: archived != 0,
		})
	}
	if err := rows.Err(); err != nil {
		return nil, false, err
	}
	return sessions, true, nil
}

// codexColumnText is rusqlite's `row.get::<_, String>`: only a TEXT value
// converts.
func codexColumnText(value any, index int) (string, error) {
	switch value := value.(type) {
	case string:
		return value, nil
	case []byte:
		// modernc returns TEXT as a string; a BLOB is not text.
		return "", fmt.Errorf("Invalid column type Blob at index: %d", index)
	case nil:
		return "", fmt.Errorf("Invalid column type Null at index: %d", index)
	case int64:
		return "", fmt.Errorf("Invalid column type Integer at index: %d", index)
	case float64:
		return "", fmt.Errorf("Invalid column type Real at index: %d", index)
	}
	return "", fmt.Errorf("Invalid column type at index: %d", index)
}

// codexColumnInteger is rusqlite's `row.get::<_, i64>` (and `bool`, which
// reads an INTEGER as nonzero): only an INTEGER value converts.
func codexColumnInteger(value any, index int) (int64, error) {
	switch value := value.(type) {
	case int64:
		return value, nil
	case bool:
		if value {
			return 1, nil
		}
		return 0, nil
	case nil:
		return 0, fmt.Errorf("Invalid column type Null at index: %d", index)
	case string:
		return 0, fmt.Errorf("Invalid column type Text at index: %d", index)
	case float64:
		return 0, fmt.Errorf("Invalid column type Real at index: %d", index)
	}
	return 0, fmt.Errorf("Invalid column type at index: %d", index)
}

// importSQLiteReadOnlyURI is the read-only SQLite URI of path, with
// rusqlite's default busy_timeout of five seconds.
func importSQLiteReadOnlyURI(path string) string {
	slashed := filepath.ToSlash(path)
	if filepath.VolumeName(path) != "" && !strings.HasPrefix(slashed, "/") {
		slashed = "/" + slashed
	}
	var uri strings.Builder
	uri.WriteString("file:")
	for i := 0; i < len(slashed); i++ {
		b := slashed[i]
		switch {
		case 'A' <= b && b <= 'Z', 'a' <= b && b <= 'z', '0' <= b && b <= '9',
			b == '-', b == '_', b == '.', b == '~', b == '/', b == ':':
			uri.WriteByte(b)
		default:
			fmt.Fprintf(&uri, "%%%02X", b)
		}
	}
	uri.WriteString("?mode=ro&_pragma=busy_timeout(5000)")
	return uri.String()
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
	entries, err := os.ReadDir(root)
	if err != nil {
		// One unreadable directory must not fail the whole listing. The rows
		// it holds cannot be offered, and naming such a session says why the
		// listing left it out.
		slog.Debug("skipped a Codex sessions directory the listing cannot read",
			"path", root, "error", core.IOErrorDisplay(err))
		return nil
	}
	for _, entry := range entries {
		path := filepath.Join(root, entry.Name())
		metadata, err := os.Lstat(path)
		if err != nil {
			return importIOError(err)
		}
		if metadata.Mode()&fs.ModeSymlink != 0 {
			continue
		}
		if metadata.IsDir() {
			if depth < codexRolloutDepth {
				if err := collectCodexCandidatePaths(path, depth+1, nativeTitles, candidates); err != nil {
					return err
				}
			}
			continue
		}
		if extension, ok := importPathExtension(path); !metadata.Mode().IsRegular() || !ok || extension != "jsonl" {
			continue
		}
		if sessionID, ok := codexRolloutIDFromPath(path); ok {
			if _, titled := nativeTitles[sessionID]; !titled {
				continue
			}
		}
		*candidates = append(*candidates, fileScanCandidate{
			path:       path,
			modifiedAt: metadata.ModTime(),
			sizeBytes:  uint64(metadata.Size()),
		})
	}
	return nil
}

// codexRolloutIDFromPath is the thread UUID a rollout's file name ends in.
func codexRolloutIDFromPath(path string) (string, bool) {
	stem, ok := importPathFileStem(path)
	if !ok || !utf8.ValidString(stem) || len(stem) < 36 {
		return "", false
	}
	start := len(stem) - 36
	// Rust's `str::get` refuses a range that does not start on a character.
	if !utf8.RuneStart(stem[start]) {
		return "", false
	}
	id := stem[start:]
	if id[8] != '-' || id[13] != '-' || id[18] != '-' || id[23] != '-' {
		return "", false
	}
	return id, true
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
	file, err := os.Open(path)
	if err != nil {
		return codexSessionSummary{}, fmt.Errorf("open Codex session %s: %w", path, importIOError(err))
	}
	defer file.Close()
	var source io.Reader = file
	if extension, ok := importPathExtension(path); ok && extension == "zst" {
		decoder, err := zstd.NewReader(bufio.NewReader(file))
		if err != nil {
			return codexSessionSummary{}, fmt.Errorf("decompress Codex session %s: %v", path, err)
		}
		defer decoder.Close()
		source = decoder
	}
	reader := bufio.NewReader(source)
	filtered := false
	subagent := false
	for range 8 {
		line, err := reader.ReadString('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return codexSessionSummary{}, importIOError(err)
		}
		if line == "" {
			break
		}
		if !utf8.ValidString(line) {
			return codexSessionSummary{}, errors.New("stream did not contain valid UTF-8")
		}
		record, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			return codexSessionSummary{}, fmt.Errorf("parse Codex session %s: %w", path, err)
		}
		if recordType, ok := importJSONStr(importJSONGet(record, "type")); !ok || recordType != "session_meta" {
			continue
		}
		// A structured source identifies a subagent, and an ephemeral thread
		// normally has no rollout path at all; the ephemeral check stays
		// defensive so a future writer cannot expose one in the picker.
		if _, ok := importJSONPointer(record, "payload", "source", "subagent"); ok {
			subagent = true
		}
		ephemeral, ephemeralOK := importJSONBool(importJSONPointer(record, "payload", "ephemeral"))
		filtered = filtered ||
			!codexSourceIsInteractive(importJSONPointer(record, "payload", "source")) ||
			(ephemeralOK && ephemeral)
		// Codex ACP loads a rollout by its payload `id`, which is also the
		// UUID embedded in the rollout filename. `session_id` can name a
		// parent thread and therefore is not necessarily resumable itself.
		idValue, idPresent := importJSONPointer(record, "payload", "id")
		if !idPresent {
			idValue, idPresent = importJSONPointer(record, "payload", "session_id")
		}
		id, hasID := importJSONStr(idValue, idPresent)
		hasID = hasID && id != ""
		historyMode := CodexHistoryModeLegacy
		if text, ok := importJSONStr(importJSONPointer(record, "payload", "history_mode")); ok {
			historyMode, err = parseCodexHistoryMode(text)
			if err != nil {
				return codexSessionSummary{}, err
			}
		}
		var metadata *codexSessionMetadata
		if hasID {
			if err := core.ValidateID("Codex session", id); err != nil {
				return codexSessionSummary{}, err
			}
			cwd, _ := importNonblank(importJSONPointer(record, "payload", "cwd"))
			gitBranch, ok := importNonblank(importJSONPointer(record, "payload", "git", "branch"))
			if !ok {
				gitBranch = "HEAD"
			}
			metadata = &codexSessionMetadata{id: id, cwd: cwd, gitBranch: gitBranch, historyMode: historyMode}
		}
		return codexSessionSummary{
			Subagent:    subagent,
			Filtered:    filtered,
			Metadata:    metadata,
			HistoryMode: historyMode,
		}, nil
	}
	return codexSessionSummary{
		Subagent:    subagent,
		Filtered:    filtered,
		Metadata:    nil,
		HistoryMode: CodexHistoryModeLegacy,
	}, nil
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
	titles := map[string]string{}
	// Older Codex stores use history only as their compact interactive-session
	// index. Keep those IDs discoverable, but do not turn prompt text into a
	// session name.
	history := filepath.Join(home, "history.jsonl")
	if importIsFile(history) {
		err := codexEachRecord(history, func(record any) {
			sessionID, hasID := importJSONStr(importJSONGet(record, "session_id"))
			text, hasText := importJSONStr(importJSONGet(record, "text"))
			if hasID && hasText && strings.TrimSpace(text) != "" {
				if _, ok := titles[sessionID]; !ok {
					titles[sessionID] = sessionID
				}
			}
		})
		if err != nil {
			return nil, err
		}
	}
	index := filepath.Join(home, "session_index.jsonl")
	if importIsFile(index) {
		err := codexEachRecord(index, func(record any) {
			sessionID, hasID := importJSONStr(importJSONGet(record, "id"))
			title, hasTitle := importJSONStr(importJSONGet(record, "thread_name"))
			if !hasID || !hasTitle {
				return
			}
			if normalized, ok := core.NormalizeSessionTitle(title); ok {
				titles[sessionID] = normalized
			}
		})
		if err != nil {
			return nil, err
		}
	}
	return titles, nil
}

// codexEachRecord parses every line of a JSONL file, failing on the first
// line that is not JSON.
func codexEachRecord(path string, visit func(record any)) error {
	file, err := os.Open(path)
	if err != nil {
		return importIOError(err)
	}
	defer file.Close()
	lines := newImportLineReader(file)
	for {
		line, ok, err := lines.next()
		if err != nil {
			return err
		}
		if !ok {
			return nil
		}
		record, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			return err
		}
		visit(record)
	}
}
