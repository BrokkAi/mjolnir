// Port of mj-core/src/confined_path.rs.

// Paths below a directory Mjolnir owns, reached without following a link
// that the directory's contents could have planted to send a read or a
// change somewhere else.

package core

import (
	"fmt"
	"os"
)

// RejectSymlinks refuses `relative` below `root` when `root`, or any part of
// `relative` under it, is a symbolic link. With `missingAllowed`, the check
// ends at the first part that does not exist, since nothing below it exists
// either.
func RejectSymlinks(root string, relative string, missingAllowed bool) error {
	panic("unported: mj-core/src/confined_path.rs reject_symlinks")
}

// ConfinedDir is a directory opened without following a link, through which
// every later read, write, and delete below it resolves one name at a time,
// never following a link at any step. A path check followed by a separate
// operation on the same path text can be redirected by renaming a checked
// directory away and planting a link in its place (#1419); resolving every
// operation from a handle that was itself opened without following a link
// leaves nothing to swap between the check and the use.
//
// `dir` is a directory handle opened with O_NOFOLLOW|O_DIRECTORY; names
// below it are resolved with openat-style calls (cap-std in Rust).
type ConfinedDir struct {
	dir  *os.File
	path string
}

// EntryKind is what a name in a ConfinedDir holds, without following a link.
type EntryKind int

const (
	EntryKindFile EntryKind = iota
	EntryKindDirectory
	// A link or a special file; confined walks never enter or read it.
	EntryKindOther
)

// ConfinedEntry is one Rust `(OsString, EntryKind)` pair from
// ConfinedDir.Entries.
type ConfinedEntry struct {
	Name string
	Kind EntryKind
}

// Publish is whether ConfinedDir.WriteAtomically may replace an existing
// file.
type Publish int

const (
	PublishReplace Publish = iota
	PublishCreateNew
)

// PublishError is a sealed interface over PublishErrorNotARegularFile,
// PublishErrorAlreadyExists and PublishErrorFailed.
type PublishError interface {
	error
	isPublishError()
}

// PublishErrorNotARegularFile: the name holds a link, a directory, or
// another non-regular file.
type PublishErrorNotARegularFile struct{}

// PublishErrorAlreadyExists: PublishCreateNew found the name already taken.
type PublishErrorAlreadyExists struct{}

type PublishErrorFailed struct{ Err error }

func (PublishErrorNotARegularFile) isPublishError() {}
func (PublishErrorAlreadyExists) isPublishError()   {}
func (PublishErrorFailed) isPublishError()          {}

func (PublishErrorNotARegularFile) Error() string { return "destination is not a regular file" }
func (PublishErrorAlreadyExists) Error() string   { return "file already exists" }

// Error renders like anyhow's `{:#}`: the whole cause chain joined by ": ",
// which wrapped Go errors already print.
func (e PublishErrorFailed) Error() string { return fmt.Sprint(e.Err) }
func (e PublishErrorFailed) Unwrap() error { return e.Err }

// ConfinedDirOpenRoot opens `root`, refusing it when it is a link. Returns
// nil when it does not exist. Directories above `root` are trusted as they
// are.
func ConfinedDirOpenRoot(root string) (*ConfinedDir, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::open_root")
}

// ConfinedDirCreateRoot opens `root` as ConfinedDirOpenRoot does, creating
// it and its missing parents first.
func ConfinedDirCreateRoot(root string) (*ConfinedDir, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::create_root")
}

func (d *ConfinedDir) Path() string { return d.path }

// OpenDir opens the directory at `relative` below this one. Returns nil when
// a part of it does not exist; refuses a part that is a link.
func (d *ConfinedDir) OpenDir(relative string) (*ConfinedDir, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::open_dir")
}

// CreateDirAll opens the directory at `relative` below this one, creating
// each missing part; refuses a part that is a link.
func (d *ConfinedDir) CreateDirAll(relative string) (*ConfinedDir, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::create_dir_all")
}

// Entries is the names directly in this directory and what each holds.
func (d *ConfinedDir) Entries() ([]ConfinedEntry, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::entries")
}

func (d *ConfinedDir) openRegularFile(name string) (*os.File, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::open_regular_file")
}

// ReadBytes reads bytes without following links or opening a non-regular
// file for reading. Returns nil, nil when the name does not exist.
func (d *ConfinedDir) ReadBytes(name string) ([]byte, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::read_bytes")
}

// ReadToString reads text without following links or opening a non-regular
// file for reading. Returns nil when the name does not exist.
func (d *ConfinedDir) ReadToString(name string) (*string, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::read_to_string")
}

// WriteAtomically publishes `body` as the file `name` in this directory
// through a temporary file in this directory, so a reader sees the old file
// or the complete new one. A new file is readable by its owner only; a
// replaced file keeps its permissions. Refuses a `name` that is not a
// regular file, such as a link, rather than replacing it.
func (d *ConfinedDir) WriteAtomically(name string, body []byte, publish Publish) PublishError {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::write_atomically")
}

// RemoveFile removes the file or link `name` in this directory; a link is
// removed itself, never what it points to.
func (d *ConfinedDir) RemoveFile(name string) error {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::remove_file")
}

// RemoveDir removes the empty directory `name` in this directory.
func (d *ConfinedDir) RemoveDir(name string) error {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::remove_dir")
}

func (d *ConfinedDir) tryClone() (*ConfinedDir, error) {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::try_clone")
}

// sync makes a rename or new entry in this directory durable. Directory
// fsync is only available on Unix, as in SyncDirectory.
func (d *ConfinedDir) sync() error {
	panic("unported: mj-core/src/confined_path.rs ConfinedDir::sync")
}

func splitRoot(root string) (string, string, error) {
	panic("unported: mj-core/src/confined_path.rs split_root")
}

func normalComponents(relative string) ([]string, error) {
	panic("unported: mj-core/src/confined_path.rs normal_components")
}

func createAndOpen(parent *os.File, name string, path string) (*os.File, error) {
	panic("unported: mj-core/src/confined_path.rs create_and_open")
}

func noFollowDirectory(path string) string {
	return fmt.Sprintf("open %s as a directory without following a link", path)
}
