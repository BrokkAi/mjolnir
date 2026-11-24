// Port of mj-import/src/import/named_session.rs.
//
// Looking one native session up by the id the user named.
//
// Every harness listing is a resume picker: it hides sessions the user is
// unlikely to want back, and it skips anything it cannot read. Neither
// applies once the user names a session by id, so each importer falls back to
// a lookup that goes straight to the path the id points at.
//
// What that lookup refuses is the same for all four harnesses, because the
// archive step behind them is the same: `collect_native_tree` stores only
// regular files and real directories inside the harness home, so anything
// reached through a symlink would either be copied from outside the home or
// archived as nothing at all. This module holds that judgement and its
// wording once, so a refusal reads the same whichever importer produced it.

package mjimport

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// namedEntry is what a lookup by session id found where the id pointed. One
// of namedEntryAbsent, namedEntryImportable or namedEntryRejected.
type namedEntry interface {
	isNamedEntry()
}

// namedEntryAbsent: nothing is there. The lookup keeps searching and may end
// up reporting the session as not found.
type namedEntryAbsent struct{}

// namedEntryImportable: the entry can be imported, with the
// `symlink_metadata` already read.
type namedEntryImportable struct {
	metadata fs.FileInfo
}

// namedEntryRejected: something is there that cannot be imported, with the
// reason to report in place of "not found".
type namedEntryRejected string

func (namedEntryAbsent) isNamedEntry()     {}
func (namedEntryImportable) isNamedEntry() {}
func (namedEntryRejected) isNamedEntry()   {}

// namedSessionStore is how one harness stores a session, so every importer
// refuses the same things in the same words.
type namedSessionStore struct {
	// harness is the harness name as error text uses it, such as
	// `Grok Build`.
	harness string
	// stored is what the harness keeps per session, named in the plural:
	// `transcripts` or `session directories`.
	stored string
}

var claudeStore = namedSessionStore{harness: "Claude", stored: "transcripts"}
var codexStore = namedSessionStore{harness: "Codex", stored: "rollouts"}
var kimiStore = namedSessionStore{harness: "Kimi", stored: "session directories"}
var grokStore = namedSessionStore{harness: "Grok Build", stored: "session directories"}

// File classifies the file a named session id points at.
func (s namedSessionStore) File(path string) namedEntry {
	return s.classify(path, false)
}

// Directory classifies the directory a named session id points at.
func (s namedSessionStore) Directory(path string) namedEntry {
	return s.classify(path, true)
}

// Container classifies the directory a session sits inside: a Claude project
// directory, a Kimi workspace directory, a Grok working directory.
// describedAs is how a refusal names it.
//
// Every listing and every by-id lookup asks this, so both agree about what
// can be imported. A symlinked container is refused, because the archive
// step follows no symlink and would store the session as nothing at all.
// Anything there that is not a directory is absent: the harness home also
// holds index and lock files, which are no container at all.
func (s namedSessionStore) Container(path string, describedAs string) namedEntry {
	entry := entryMetadata(path)
	importable, ok := entry.(namedEntryImportable)
	if !ok {
		return entry
	}
	if importable.metadata.Mode()&fs.ModeSymlink != 0 {
		return namedEntryRejected(s.SymlinkedContainer(path, describedAs))
	}
	if !importable.metadata.IsDir() {
		return namedEntryAbsent{}
	}
	return importable
}

func (s namedSessionStore) classify(path string, wantDirectory bool) namedEntry {
	entry := entryMetadata(path)
	importable, ok := entry.(namedEntryImportable)
	if !ok {
		return entry
	}
	metadata := importable.metadata
	if metadata.Mode()&fs.ModeSymlink != 0 {
		return namedEntryRejected(fmt.Sprintf("%s is a symlink; %s", path, s.onlyStoredDirectly()))
	}
	if wantDirectory {
		if !metadata.IsDir() {
			return namedEntryRejected(fmt.Sprintf("%s is not a directory", path))
		}
	} else if !metadata.Mode().IsRegular() {
		return namedEntryRejected(fmt.Sprintf("%s is not a regular file", path))
	}
	return importable
}

// SymlinkedContainer is the refusal for a symlinked directory the named
// session sits inside, such as a Claude project directory or a Grok working
// directory. describedAs is how the message names that directory.
func (s namedSessionStore) SymlinkedContainer(path string, describedAs string) string {
	return fmt.Sprintf("%s is a symlinked %s; %s", path, describedAs, s.onlyStoredDirectly())
}

// NoCwd is the refusal for a session whose own records never say which
// directory it ran in. The archive is collected relative to that directory,
// so there is nothing to import without it.
func (s namedSessionStore) NoCwd(path string) string {
	return fmt.Sprintf("%s session %s has no cwd", s.harness, path)
}

// CannotImport is the error a lookup reports when everything at the named
// path was refused. It names the reasons instead of saying the session is
// missing.
func (s namedSessionStore) CannotImport(nativeSessionID string, rejected []string) error {
	return fmt.Errorf("%s session %s cannot be imported: %s",
		s.harness, core.RustDebugString(nativeSessionID), strings.Join(rejected, "; "))
}

func (s namedSessionStore) onlyStoredDirectly() string {
	return fmt.Sprintf("Mjolnir imports only %s stored directly in the %s home", s.stored, s.harness)
}

// entryMetadata reads one path's own metadata, without following a symlink.
// A path that is not there is absent, so the caller keeps looking; one that
// cannot be read at all is refused by name. Importable only carries the
// metadata back: what that metadata describes is still for the caller to
// judge.
func entryMetadata(path string) namedEntry {
	metadata, err := os.Lstat(path)
	if err == nil {
		return namedEntryImportable{metadata: metadata}
	}
	if errors.Is(err, fs.ErrNotExist) {
		return namedEntryAbsent{}
	}
	return namedEntryRejected(cannotRead(path, err))
}

// cannotRead is the refusal for a path a lookup cannot read at all, such as
// a directory with no permissions. A lookup reports this instead of failing,
// so naming the session says which path is in the way rather than returning
// a bare I/O error or calling the session missing.
func cannotRead(path string, err error) string {
	return fmt.Sprintf("%s cannot be read: %s", path, core.IOErrorDisplay(err))
}
