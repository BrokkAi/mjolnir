// Port of brokk-sessionwiki 0.34.28 src/adapters/mod.rs.

package brokksessionwiki

import (
	"errors"
	"time"
)

// StoreKey is one session of a shared store: its stable key, which doubles
// as the session's stored path, and its change token.
type StoreKey struct {
	Key   string
	Token int64
}

// Store is a store that holds many sessions in one place rather than one
// file per session. The indexer enumerates sessions from Keys, re-parsing
// only changed ones, instead of calling Discover and Parse.
type Store struct {
	// Keys is every session, cheap to compute without a full parse.
	Keys []StoreKey
	// Files are the backing files, for `scan` size accounting.
	Files []string
	// HadError: a backing store existed but could not be read this run. The
	// indexer then skips deletion reconciliation.
	HadError bool
}

// Discovered is the result of file discovery. HadError means the listing
// is partial; the indexer then skips deletion reconciliation.
type Discovered struct {
	Files    []string
	HadError bool
}

// Adapter is one supported agent tool: where it keeps its sessions and how
// to parse one into a Session. Embed AdapterDefaults for the methods the
// Rust trait gives default bodies.
type Adapter interface {
	// NativeID is the literal harness identity, when it can be resolved from
	// the key or source.
	NativeID(key string) (string, bool)
	Name() string
	// Root is the store root. It may not exist on this machine.
	Root() (string, bool)
	// Discover lists every session file under the root.
	Discover() Discovered
	// Parse parses one session file. It must never panic on malformed input.
	Parse(path string) (*Session, error)
	// Store is the shared store for tools that do not use one file per
	// session; nil for an ordinary file-per-session adapter.
	Store() *Store
	// ParseKey parses one session of a shared store by its key.
	ParseKey(key string) (*Session, error)
	// ModifiedAt is the actual last activity time of a shared-store key.
	ModifiedAt(key string) (time.Time, bool)
	// PathKeyDepth opts a filesystem shared store into alias-aware indexing.
	PathKeyDepth() (int, bool)
	// ReconcileScope limits deletion reconciliation to indexed rows whose key
	// starts with the returned prefix.
	ReconcileScope() (string, bool)
}

// AdapterDefaults holds the Adapter methods the Rust trait implements by
// default.
type AdapterDefaults struct{}

func (AdapterDefaults) NativeID(string) (string, bool) { return "", false }
func (AdapterDefaults) Store() *Store                  { return nil }
func (AdapterDefaults) ParseKey(string) (*Session, error) {
	return nil, errors.New("this adapter is not a shared store")
}
func (AdapterDefaults) ModifiedAt(string) (time.Time, bool) { return time.Time{}, false }
func (AdapterDefaults) PathKeyDepth() (int, bool)           { return 0, false }
func (AdapterDefaults) ReconcileScope() (string, bool)      { return "", false }

// unportedAdapter stands in for the Adapter methods of an adapter whose
// module is not ported yet.
type unportedAdapter struct {
	module string
}

func (a unportedAdapter) NativeID(string) (string, bool) {
	panic("unported: brokk-sessionwiki " + a.module + " native_id")
}
func (a unportedAdapter) Name() string { panic("unported: brokk-sessionwiki " + a.module + " name") }
func (a unportedAdapter) Root() (string, bool) {
	panic("unported: brokk-sessionwiki " + a.module + " root")
}
func (a unportedAdapter) Discover() Discovered {
	panic("unported: brokk-sessionwiki " + a.module + " discover")
}
func (a unportedAdapter) Parse(string) (*Session, error) {
	panic("unported: brokk-sessionwiki " + a.module + " parse")
}
func (a unportedAdapter) Store() *Store { panic("unported: brokk-sessionwiki " + a.module + " store") }
func (a unportedAdapter) ParseKey(string) (*Session, error) {
	panic("unported: brokk-sessionwiki " + a.module + " parse_key")
}
func (a unportedAdapter) ModifiedAt(string) (time.Time, bool) {
	panic("unported: brokk-sessionwiki " + a.module + " modified_at")
}
func (a unportedAdapter) PathKeyDepth() (int, bool) {
	panic("unported: brokk-sessionwiki " + a.module + " path_key_depth")
}
func (a unportedAdapter) ReconcileScope() (string, bool) {
	panic("unported: brokk-sessionwiki " + a.module + " reconcile_scope")
}

var (
	_ Adapter = (*Codex)(nil)
	_ Adapter = (*CodexHistory)(nil)
	_ Adapter = (*ClaudeCode)(nil)
	_ Adapter = (*KimiCode)(nil)
	_ Adapter = (*GrokBuild)(nil)
	_ Adapter = (*Muse)(nil)
	_ Adapter = (*Dsh)(nil)
	_ Adapter = (*Antigravity)(nil)
)

// ParseSessionKey parses one key from a shared-store adapter and applies the
// same normalization as the ordinary file-backed path.
func ParseSessionKey(adapter Adapter, key string) (*Session, error) {
	session, err := adapter.ParseKey(key)
	if err != nil {
		return nil, err
	}
	FoldToolParts(session)
	return session, nil
}

// All is every registered adapter with its stock location.
func All() []Adapter {
	panic("unported: brokk-sessionwiki src/adapters/mod.rs all")
}
