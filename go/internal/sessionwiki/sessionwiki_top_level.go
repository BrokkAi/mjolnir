// Port of mj-sessionwiki/src/sessionwiki/top_level.rs.
//
// Select native parents before handing discovered files to SessionWiki.

package sessionwiki

import (
	"fmt"
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
	panic("unported: mj-sessionwiki/src/sessionwiki/top_level.rs Snapshot::from_state")
}

// ownsIndexedChild: SessionWiki derives its key from the full path, while
// Mjolnir tracks native IDs. Match the native ID so standalone sync cannot
// expose a child transcript under another tool's partition.
func (s *topLevelSnapshot) ownsIndexedChild(sessionID, path, tool, kind string) bool {
	panic("unported: mj-sessionwiki/src/sessionwiki/top_level.rs Snapshot::owns_indexed_child")
}

func (s *topLevelSnapshot) ownsSession(sessionID string) bool {
	_, ok := s.childSessionIDs[sessionID]
	return ok
}

func (s *topLevelSnapshot) isKnownTopLevel(sessionID string) bool {
	_, ok := s.topLevelSessionIDs[sessionID]
	return ok
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
	panic("unported: mj-sessionwiki/src/sessionwiki/top_level.rs is_child")
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
	panic("unported: mj-sessionwiki/src/sessionwiki/top_level.rs prepare")
}

// topLevelPrune is Rust's `prune`: excluded rows must be removed before
// reconciliation; otherwise the library archives them and leaves their
// transcripts searchable. Include archive-only copies so a later cache
// rebuild cannot bring a child back. It returns how many sessions it
// removed.
func topLevelPrune(conn *store.Connection, excluded map[string]struct{}, ownership *topLevelSnapshot) (int, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki/top_level.rs prune")
}
