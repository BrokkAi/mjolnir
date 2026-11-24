// Port of mj-import/src/import/native.rs.

package mjimport

import (
	"errors"
	"fmt"
	"math"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// cachedNativeMetadata is what one scanned native session file parsed into,
// keyed by the file itself. One of cachedNativeMetadataClaude,
// cachedNativeMetadataCodex or cachedNativeMetadataIndexEligible.
type cachedNativeMetadata interface {
	isCachedNativeMetadata()
}

// cachedNativeMetadataClaude is `claude_native_metadata`, including its
// "filtered out" verdict (a nil metadata).
type cachedNativeMetadataClaude struct {
	metadata *claudeListing
}

// cachedNativeMetadataCodex is `codex_session_metadata`, including its "not
// interactive" verdict (a nil metadata).
type cachedNativeMetadataCodex struct {
	metadata *codexSessionMetadata
}

type cachedNativeMetadataIndexEligible bool

func (cachedNativeMetadataClaude) isCachedNativeMetadata()        {}
func (cachedNativeMetadataCodex) isCachedNativeMetadata()         {}
func (cachedNativeMetadataIndexEligible) isCachedNativeMetadata() {}

type cachedNativeEntry struct {
	modifiedAt time.Time
	sizeBytes  uint64
	metadata   cachedNativeMetadata
}

type nativeScanCacheInner struct {
	entries     map[string]cachedNativeEntry
	parsedFiles uint64
}

// NativeScanCache remembers what each native session file parsed into, keyed
// by its path, modified time and size. The parsers are pure functions of a
// file's content, so an unchanged file never has to be opened again. One
// cache lives for the process, so reopening the resume dialog reparses only
// what changed.
//
// Rust's cache is a cloneable handle to shared state; share a
// *NativeScanCache instead. The zero value is an empty cache. Rust's
// `lock` (which recovers a poisoned mutex) is mu.
type NativeScanCache struct {
	mu    sync.Mutex
	inner nativeScanCacheInner
}

func NewNativeScanCache() *NativeScanCache {
	return &NativeScanCache{}
}

var sharedNativeScanCache = NewNativeScanCache()

// NativeScanCacheShared is the cache shared by index discovery and
// Mjolnir's SessionWiki reads.
func NativeScanCacheShared() *NativeScanCache {
	return sharedNativeScanCache
}

// ParsedFiles is how many files this cache has actually parsed, for tests
// and diagnostics.
func (c *NativeScanCache) ParsedFiles() uint64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.inner.parsedFiles
}

// IndexEligible reports whether index discovery lists a session file. It
// excludes identified children, preserving parents even when their
// entrypoint or missing working directory prevents importing.
func (c *NativeScanCache) IndexEligible(kind core.HarnessKind, path string) (bool, error) {
	panic("unported: mj-import/src/import/native.rs NativeScanCache::index_eligible")
}

func (c *NativeScanCache) cached(path string, modifiedAt time.Time, sizeBytes uint64) (cachedNativeMetadata, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	entry, ok := c.inner.entries[path]
	if !ok || !entry.modifiedAt.Equal(modifiedAt) || entry.sizeBytes != sizeBytes {
		return nil, false
	}
	return entry.metadata, true
}

func (c *NativeScanCache) store(path string, modifiedAt time.Time, sizeBytes uint64, metadata cachedNativeMetadata) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.inner.parsedFiles < math.MaxUint64 {
		c.inner.parsedFiles++
	}
	if c.inner.entries == nil {
		c.inner.entries = map[string]cachedNativeEntry{}
	}
	c.inner.entries[path] = cachedNativeEntry{modifiedAt: modifiedAt, sizeBytes: sizeBytes, metadata: metadata}
}

// claudeMetadata is Claude metadata for one transcript, parsing only on a
// cache miss. Errors are returned to the caller and never cached.
func (c *NativeScanCache) claudeMetadata(
	path string,
	modifiedAt time.Time,
	sizeBytes uint64,
	parse func() (*claudeListing, error),
) (*claudeListing, error) {
	if cached, ok := c.cached(path, modifiedAt, sizeBytes); ok {
		if claude, ok := cached.(cachedNativeMetadataClaude); ok {
			return claude.metadata, nil
		}
	}
	metadata, err := parse()
	if err != nil {
		return nil, err
	}
	c.store(path, modifiedAt, sizeBytes, cachedNativeMetadataClaude{metadata: metadata})
	return metadata, nil
}

// codexMetadata is Codex metadata for one rollout, parsing only on a cache
// miss.
func (c *NativeScanCache) codexMetadata(
	path string,
	modifiedAt time.Time,
	sizeBytes uint64,
	parse func() (*codexSessionMetadata, error),
) (*codexSessionMetadata, error) {
	if cached, ok := c.cached(path, modifiedAt, sizeBytes); ok {
		if codex, ok := cached.(cachedNativeMetadataCodex); ok {
			return codex.metadata, nil
		}
	}
	metadata, err := parse()
	if err != nil {
		return nil, err
	}
	c.store(path, modifiedAt, sizeBytes, cachedNativeMetadataCodex{metadata: metadata})
	return metadata, nil
}

// LocatedNativeSession is one native session located on disk, normalized
// across harnesses: the id `session/load` takes and the file or directory
// its transcript is read from.
type LocatedNativeSession struct {
	NativeSessionID string
	SourcePath      string
}

// NativeSessionListing is one native session as a picker lists it,
// normalized across harnesses.
type NativeSessionListing struct {
	NativeSessionID string
	Title           string
	ModifiedAt      time.Time
	GitBranch       string
	SizeBytes       uint64
	Cwd             string
	// UnavailableReason is why this session cannot be imported, when it
	// cannot be; nil otherwise.
	UnavailableReason *string
	// NativelyArchived is archived inside the harness itself. Only Codex
	// reports this today.
	NativelyArchived bool
}

// LocateNativeSession locates one native session for any harness.
func LocateNativeSession(
	harness core.HarnessKind,
	home string,
	selection ClaudeSessionSelection,
) (LocatedNativeSession, error) {
	panic("unported: mj-import/src/import/native.rs locate_native_session")
}

// NativeSessionIDFromPath is the id LocateNativeSession takes, read back
// from the path that locator produced. false when the path has no readable
// name.
//
// The inverse of the locators above, so every caller that has only a
// transcript path — the SessionWiki index stores one per row — names the
// session the same way `mj import` does. Each harness keeps its id in a
// different part of the path: Claude Code names the file after the session,
// Codex appends the thread UUID to a `rollout-` prefix, Kimi and Grok name
// the session's own directory, and Muse names the directory above
// `session.jsonl`.
func NativeSessionIDFromPath(harness core.HarnessKind, path string) (string, bool) {
	panic("unported: mj-import/src/import/native.rs native_session_id_from_path")
}

// ReadNativeTranscript projects one native session into the canonical
// transcript, for any harness.
func ReadNativeTranscript(harness core.HarnessKind, sourcePath string) (ClaudeTranscript, error) {
	switch harness {
	case core.HarnessKindMuse:
		return readMuseTranscript(sourcePath, sourcePath)
	case core.HarnessKindCodex:
		return ReadCodexTranscript(sourcePath)
	case core.HarnessKindClaude:
		return ReadClaudeTranscript(sourcePath)
	case core.HarnessKindKimi:
		return ReadKimiTranscript(sourcePath)
	case core.HarnessKindGrok:
		return ReadGrokTranscript(sourcePath)
	case core.HarnessKindOpenCode:
		return ClaudeTranscript{}, errors.New("OpenCode keeps its sessions in a SQLite database; Mjolnir cannot import them yet")
	case core.HarnessKindAntigravity:
		return ClaudeTranscript{}, errors.New("Mjolnir cannot import Antigravity sessions yet")
	case core.HarnessKindDsh:
		return ClaudeTranscript{}, errors.New("Mjolnir cannot import DSH sessions yet")
	}
	panic(fmt.Sprintf("unknown harness kind %q", string(harness)))
}

// NativeSessionsEnumerable reports whether Mjolnir can list this harness's
// native sessions at all. The harnesses that keep sessions in a database, and
// DSH, cannot be scanned yet; callers skip them instead of reporting the scan
// as a failure.
func NativeSessionsEnumerable(harness core.HarnessKind) bool {
	switch harness {
	case core.HarnessKindOpenCode, core.HarnessKindDsh, core.HarnessKindAntigravity:
		return false
	}
	return true
}

// ScanNativeSessions scans a harness home newest first, reporting after
// every candidate.
func ScanNativeSessions(
	harness core.HarnessKind,
	home string,
	cache *NativeScanCache,
	report func(SessionScanProgress[NativeSessionListing]),
) error {
	panic("unported: mj-import/src/import/native.rs scan_native_sessions")
}
