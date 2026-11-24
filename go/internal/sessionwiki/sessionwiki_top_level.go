// Port of mj-sessionwiki/src/sessionwiki/top_level.rs.
//
// Select native parents before handing discovered files to SessionWiki.

package sessionwiki

import (
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/mjimport"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// preparedFiles is an adapter whose discovery has already run, with its
// sub-agent files removed.
type preparedFiles struct {
	adapter    brokksessionwiki.Adapter
	discovered brokksessionwiki.Discovered
}

// topLevelSnapshot is Rust's `top_level::Snapshot`: one ownership view
// shared by index discovery, cleanup, and every Mjolnir read of the shared
// SessionWiki index. Its sets are not changed after it is built, so copies
// may share them.
type topLevelSnapshot struct {
	childSessionIDs map[string]struct{}
	childNativeIDs  map[string]struct{}
	// topLevelSessionIDs is the top-level sessions Mjolnir records, whether
	// or not they are indexed.
	topLevelSessionIDs map[string]struct{}
}

// topLevelSnapshotFromState is Rust's Snapshot::from_state.
func topLevelSnapshotFromState(state *core.StateState) topLevelSnapshot {
	childSessionIDs := map[string]struct{}{}
	for id := range state.Subagents.Keys() {
		childSessionIDs[id] = struct{}{}
	}
	for id := range state.Sessions.Keys() {
		if state.IsSubagentSession(id) {
			childSessionIDs[id] = struct{}{}
		}
	}
	childNativeIDs := map[string]struct{}{}
	for id := range childSessionIDs {
		if record := state.Sessions.GetShared(id); record != nil && record.NativeSessionID != nil {
			childNativeIDs[topLevelASCIILowercase(*record.NativeSessionID)] = struct{}{}
		}
	}
	topLevelSessionIDs := map[string]struct{}{}
	for id := range state.Sessions.Keys() {
		if _, child := childSessionIDs[id]; !child {
			topLevelSessionIDs[id] = struct{}{}
		}
	}
	return topLevelSnapshot{
		childSessionIDs:    childSessionIDs,
		childNativeIDs:     childNativeIDs,
		topLevelSessionIDs: topLevelSessionIDs,
	}
}

// ownsIndexedChild: SessionWiki derives its key from the full path, while
// Mjolnir tracks native IDs. Match the native ID so standalone sync cannot
// expose a child transcript under another tool's partition.
func (s *topLevelSnapshot) ownsIndexedChild(sessionID, path, tool, kind string) bool {
	if kind != "main" {
		return true
	}
	if _, owned := s.childSessionIDs[sessionID]; tool == mjolnirTool && owned {
		return true
	}
	if _, owned := s.childNativeIDs[topLevelASCIILowercase(sessionID)]; owned {
		return true
	}
	if nativeID, ok := brokksessionwiki.NativeIDOf(path); ok {
		if _, owned := s.childNativeIDs[nativeID]; owned {
			return true
		}
	}
	return false
}

func (s *topLevelSnapshot) ownsSession(sessionID string) bool {
	_, ok := s.childSessionIDs[sessionID]
	return ok
}

func (s *topLevelSnapshot) isKnownTopLevel(sessionID string) bool {
	_, ok := s.topLevelSessionIDs[sessionID]
	return ok
}

// topLevelASCIILowercase is Rust's str::to_ascii_lowercase.
func topLevelASCIILowercase(text string) string {
	return strings.Map(func(character rune) rune {
		if 'A' <= character && character <= 'Z' {
			return character + ('a' - 'A')
		}
		return character
	}, text)
}

// currentSnapshot loads the ownership snapshot used to classify rows a
// standalone SessionWiki sync may have discovered independently.
func currentSnapshot() (topLevelSnapshot, error) {
	state, err := store.LoadState()
	if err != nil {
		return topLevelSnapshot{}, fmt.Errorf("load session ownership for SessionWiki: %w", err)
	}
	return topLevelSnapshotFromState(&state), nil
}

// nativeIsChild reports whether a native Claude or Codex transcript is a
// child, using the same bounded summary parser and cache as sync-side
// discovery.
func nativeIsChild(kind core.HarnessKind, path string, cache *mjimport.NativeScanCache) (bool, error) {
	eligible, err := cache.IndexEligible(kind, path)
	if err != nil {
		return false, err
	}
	return !eligible, nil
}

// storeKeySeparator is the separator SessionWiki puts between a store's path
// and an entry in it.
const storeKeySeparator = '\x1f'

// isChild classifies one row as a child. SessionWiki's `kind` remains one
// signal, while Mjolnir native-id ownership and the shared native summary
// classifier cover rows a standalone sync may have mislabeled as `main`.
func isChild(row *brokksessionwiki.SessionRow, snapshot *topLevelSnapshot, cache *mjimport.NativeScanCache) (bool, error) {
	if snapshot.ownsIndexedChild(row.SessionID, row.Path, row.Tool, row.Kind) {
		return true, nil
	}
	kind, ok := harnessForTool(row.Tool)
	if !ok || (kind != core.HarnessKindCodex && kind != core.HarnessKindClaude) {
		return false, nil
	}
	if kind == core.HarnessKindClaude && slices.Contains(pathComponents(row.Path), "subagents") {
		return true, nil
	}
	// Store keys and retained transcripts need not name a file. Their indexed
	// kind and ownership above remain the classification signals. A store key
	// joins its database path and entry with U+001F, which Windows rejects as
	// a path, so it is never probed as a file.
	if _, ok := brokksessionwiki.NativeIDOf(row.Path); !ok || strings.ContainsRune(row.Path, storeKeySeparator) {
		return false, nil
	}
	exists, err := pathTryExists(row.Path)
	if err != nil {
		return false, fmt.Errorf("check indexed source %s: %w", row.Path, err)
	}
	if !exists {
		return false, nil
	}
	child, err := nativeIsChild(kind, row.Path, cache)
	if err != nil {
		return false, fmt.Errorf("classify indexed %s session %s: %w", row.Tool, row.SessionID, err)
	}
	return child, nil
}

// pathTryExists is Rust's Path::try_exists: false only when the path is
// confirmed absent, an error when its existence cannot be told.
func pathTryExists(path string) (bool, error) {
	_, err := os.Stat(path)
	switch {
	case err == nil:
		return true, nil
	case errors.Is(err, fs.ErrNotExist):
		return false, nil
	}
	return false, err
}

func (p *preparedFiles) NativeID(key string) (string, bool) { return p.adapter.NativeID(key) }
func (p *preparedFiles) Name() string                       { return p.adapter.Name() }
func (p *preparedFiles) Root() (string, bool)               { return p.adapter.Root() }
func (p *preparedFiles) ReconcileScope() (string, bool)     { return p.adapter.ReconcileScope() }
func (p *preparedFiles) Discover() brokksessionwiki.Discovered {
	return brokksessionwiki.Discovered{
		Files:    append([]string(nil), p.discovered.Files...),
		HadError: p.discovered.HadError,
	}
}
func (p *preparedFiles) Parse(path string) (*brokksessionwiki.Session, error) {
	return p.adapter.Parse(path)
}
func (p *preparedFiles) Store() *brokksessionwiki.Store { return p.adapter.Store() }
func (p *preparedFiles) ParseKey(key string) (*brokksessionwiki.Session, error) {
	return p.adapter.ParseKey(key)
}
func (p *preparedFiles) ModifiedAt(key string) (time.Time, bool) { return p.adapter.ModifiedAt(key) }
func (p *preparedFiles) PathKeyDepth() (int, bool)               { return p.adapter.PathKeyDepth() }

// topLevelPrepare is Rust's `prepare`: discovery and removal consume the
// same classification snapshot. An unreadable source is neither indexed nor
// classified for deletion, and prevents reconciliation of its incomplete
// listing. The second result is the excluded paths.
func topLevelPrepare(adapters []brokksessionwiki.Adapter, cache *mjimport.NativeScanCache) ([]brokksessionwiki.Adapter, map[string]struct{}) {
	prepared := make([]brokksessionwiki.Adapter, 0, len(adapters))
	excluded := map[string]struct{}{}
	for _, adapter := range adapters {
		kind, ok := harnessForTool(adapter.Name())
		if !ok || (kind != core.HarnessKindCodex && kind != core.HarnessKindClaude) {
			prepared = append(prepared, adapter)
			continue
		}
		discovered := adapter.Discover()
		hadError := discovered.HadError
		selected := make([]string, 0, len(discovered.Files))
		for _, path := range discovered.Files {
			child, err := nativeIsChild(kind, path, cache)
			switch {
			case err != nil:
				hadError = true
				slog.Warn("could not classify a native session for SessionWiki",
					"tool", adapter.Name(), "path", path, "error", err)
			case child:
				excluded[path] = struct{}{}
			default:
				selected = append(selected, path)
			}
		}
		prepared = append(prepared, &preparedFiles{
			adapter:    adapter,
			discovered: brokksessionwiki.Discovered{Files: selected, HadError: hadError},
		})
	}
	return prepared, excluded
}

// topLevelPrune is Rust's `prune`: excluded rows must be removed before
// reconciliation; otherwise the library archives them and leaves their
// transcripts searchable. Include archive-only copies so a later cache
// rebuild cannot bring a child back. It returns how many sessions it
// removed.
func topLevelPrune(conn *store.Connection, excluded map[string]struct{}, ownership *topLevelSnapshot) (int, error) {
	// Classification of existing rows and deletion share one SQLite snapshot.
	transaction, err := conn.Begin()
	if err != nil {
		return 0, err
	}
	defer transaction.Rollback()
	found, err := topLevelPruneCandidates(transaction, excluded, ownership)
	if err != nil {
		return 0, err
	}
	if len(found) == 0 {
		return 0, nil
	}
	ids := make([]string, 0, len(found))
	for id := range found {
		ids = append(ids, id)
	}
	slices.Sort(ids)
	for _, id := range ids {
		// FTS5 external-content tables need the old text before deleting it.
		if _, err := transaction.Exec(
			`INSERT INTO msgs(msgs, rowid, text)
             SELECT 'delete', id, text FROM messages WHERE session_id = ?1`,
			id,
		); err != nil {
			return 0, fmt.Errorf("remove a child session from the full-text index: %w", err)
		}
		for _, table := range []string{
			"messages",
			"touched",
			"edits",
			"archive",
			"tags",
			"notes",
			"summaries",
			"files",
		} {
			if _, err := transaction.Exec(fmt.Sprintf("DELETE FROM %s WHERE session_id = ?1", table), id); err != nil {
				return 0, fmt.Errorf("remove child session %s from %s: %w", id, table, err)
			}
		}
	}
	if err := transaction.Commit(); err != nil {
		return 0, fmt.Errorf("commit top-level-only index cleanup: %w", err)
	}
	slog.Info("removed sub-agent sessions from SessionWiki", "sessions", len(ids))
	return len(ids), nil
}

// topLevelPruneCandidates is the ids of every cached or archived row that
// excluded names or ownership classifies as a child.
func topLevelPruneCandidates(transaction *store.Transaction, excluded map[string]struct{}, ownership *topLevelSnapshot) (map[string]struct{}, error) {
	statement, err := transaction.Prepare(
		`SELECT session_id, path, kind, tool FROM files
             UNION SELECT session_id, path, kind, tool FROM archive`)
	if err != nil {
		return nil, err
	}
	defer statement.Close()
	rows, err := statement.Query()
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	ids := map[string]struct{}{}
	for rows.Next() {
		var id, path, kind, tool string
		if err := rows.Scan(&id, &path, &kind, &tool); err != nil {
			return nil, err
		}
		if _, named := excluded[path]; named || ownership.ownsIndexedChild(id, path, tool, kind) {
			ids[id] = struct{}{}
		}
	}
	return ids, rows.Err()
}
