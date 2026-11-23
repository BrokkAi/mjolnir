// Port of mj-core/src/confined_path.rs.

// Paths below a directory Mjolnir owns, reached without following a link
// that the directory's contents could have planted to send a read or a
// change somewhere else.
//
// The handle operations (openat-style calls in place of Rust's cap-std) are
// in confined_path_unix.go and confined_path_windows.go.

package core

import (
	"bytes"
	"crypto/rand"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"math"
	"os"
	"unicode/utf8"
)

// RejectSymlinks refuses `relative` below `root` when `root`, or any part of
// `relative` under it, is a symbolic link. With `missingAllowed`, the check
// ends at the first part that does not exist, since nothing below it exists
// either.
func RejectSymlinks(root string, relative string, missingAllowed bool) error {
	if info, err := os.Lstat(root); err == nil && info.Mode()&fs.ModeSymlink != 0 {
		return fmt.Errorf("%s is a symbolic link", root)
	}
	current := root
	for _, component := range pathComponents(relative) {
		current = rustPathPush(current, component.text)
		info, err := os.Lstat(current)
		switch {
		case err == nil && info.Mode()&fs.ModeSymlink != 0:
			return fmt.Errorf("%s reaches %s through a symbolic link", rustPathPush(root, relative), current)
		case err == nil:
		case missingAllowed && errors.Is(err, fs.ErrNotExist):
			return nil
		default:
			return fmt.Errorf("inspect %s: %w", current, err)
		}
	}
	return nil
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
// PublishErrorAlreadyExists, PublishErrorLengthMismatch and
// PublishErrorFailed.
type PublishError interface {
	error
	isPublishError()
}

// PublishErrorNotARegularFile: the name holds a link, a directory, or
// another non-regular file.
type PublishErrorNotARegularFile struct{}

// PublishErrorAlreadyExists: PublishCreateNew found the name already taken.
type PublishErrorAlreadyExists struct{}

// PublishErrorLengthMismatch: streamed input did not match the declared
// length.
type PublishErrorLengthMismatch struct {
	Expected uint64
	Received uint64
}

type PublishErrorFailed struct{ Err error }

func (PublishErrorNotARegularFile) isPublishError() {}
func (PublishErrorAlreadyExists) isPublishError()   {}
func (PublishErrorLengthMismatch) isPublishError()  {}
func (PublishErrorFailed) isPublishError()          {}

func (PublishErrorNotARegularFile) Error() string { return "destination is not a regular file" }
func (PublishErrorAlreadyExists) Error() string   { return "file already exists" }
func (e PublishErrorLengthMismatch) Error() string {
	return fmt.Sprintf("incomplete file upload: expected %d bytes, received %d", e.Expected, e.Received)
}

// Error renders like anyhow's `{:#}`: the whole cause chain joined by ": ",
// which wrapped Go errors already print.
func (e PublishErrorFailed) Error() string { return fmt.Sprint(e.Err) }
func (e PublishErrorFailed) Unwrap() error { return e.Err }

// ConfinedDirOpenRoot opens `root`, refusing it when it is a link. Returns
// nil when it does not exist. Directories above `root` are trusted as they
// are.
func ConfinedDirOpenRoot(root string) (*ConfinedDir, error) {
	parentPath, name, err := splitRoot(root)
	if err != nil {
		return nil, err
	}
	parent, err := os.Open(parentPath)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", parentPath, err)
	}
	defer parent.Close()
	dir, err := openDirNoFollow(parent, name)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("%s: %w", noFollowDirectory(root), err)
	}
	return &ConfinedDir{dir: dir, path: root}, nil
}

// ConfinedDirCreateRoot opens `root` as ConfinedDirOpenRoot does, creating
// it and its missing parents first.
func ConfinedDirCreateRoot(root string) (*ConfinedDir, error) {
	parentPath, name, err := splitRoot(root)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(parentPath, 0o777); err != nil {
		return nil, fmt.Errorf("create %s: %w", parentPath, err)
	}
	parent, err := os.Open(parentPath)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", parentPath, err)
	}
	defer parent.Close()
	dir, err := createAndOpen(parent, name, root)
	if err != nil {
		return nil, err
	}
	return &ConfinedDir{dir: dir, path: root}, nil
}

func (d *ConfinedDir) Path() string { return d.path }

// Close releases the directory handle (Rust's Drop).
func (d *ConfinedDir) Close() error { return d.dir.Close() }

// OpenDir opens the directory at `relative` below this one. Returns nil when
// a part of it does not exist; refuses a part that is a link.
func (d *ConfinedDir) OpenDir(relative string) (*ConfinedDir, error) {
	names, err := normalComponents(relative)
	if err != nil {
		return nil, err
	}
	current, err := d.tryClone()
	if err != nil {
		return nil, err
	}
	for _, name := range names {
		path := rustPathPush(current.path, name)
		dir, err := openDirNoFollow(current.dir, name)
		current.dir.Close()
		if errors.Is(err, fs.ErrNotExist) {
			return nil, nil
		}
		if err != nil {
			return nil, fmt.Errorf("%s: %w", noFollowDirectory(path), err)
		}
		current = &ConfinedDir{dir: dir, path: path}
	}
	return current, nil
}

// CreateDirAll opens the directory at `relative` below this one, creating
// each missing part; refuses a part that is a link.
func (d *ConfinedDir) CreateDirAll(relative string) (*ConfinedDir, error) {
	names, err := normalComponents(relative)
	if err != nil {
		return nil, err
	}
	current, err := d.tryClone()
	if err != nil {
		return nil, err
	}
	for _, name := range names {
		path := rustPathPush(current.path, name)
		dir, err := createAndOpen(current.dir, name, path)
		current.dir.Close()
		if err != nil {
			return nil, err
		}
		current = &ConfinedDir{dir: dir, path: path}
	}
	return current, nil
}

// Entries is the names directly in this directory and what each holds.
func (d *ConfinedDir) Entries() ([]ConfinedEntry, error) {
	listing, err := reopenDir(d.dir, d.path)
	if err != nil {
		return nil, fmt.Errorf("list %s: %w", d.path, err)
	}
	defer listing.Close()
	names, err := listing.ReadDir(-1)
	if err != nil {
		return nil, fmt.Errorf("list %s: %w", d.path, err)
	}
	entries := make([]ConfinedEntry, 0, len(names))
	for _, entry := range names {
		kind := EntryKindOther
		mode, err := lstatAt(d.dir, entry.Name())
		if err != nil {
			return nil, fmt.Errorf("inspect %s: %w", rustPathPush(d.path, entry.Name()), err)
		}
		switch {
		case mode&fs.ModeSymlink != 0:
		case mode.IsDir():
			kind = EntryKindDirectory
		case mode.IsRegular():
			kind = EntryKindFile
		}
		entries = append(entries, ConfinedEntry{Name: entry.Name(), Kind: kind})
	}
	return entries, nil
}

func (d *ConfinedDir) openRegularFile(name string) (*os.File, error) {
	path := rustPathPush(d.path, name)
	file, err := openFileNoFollow(d.dir, name)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("open %s without following a link: %w", path, err)
	}
	info, err := file.Stat()
	if err != nil {
		file.Close()
		return nil, fmt.Errorf("inspect %s: %w", path, err)
	}
	if !info.Mode().IsRegular() {
		file.Close()
		return nil, fmt.Errorf("%s is not a regular file", path)
	}
	return file, nil
}

// ReadBytes reads bytes without following links or opening a non-regular
// file for reading. Returns nil, nil when the name does not exist.
func (d *ConfinedDir) ReadBytes(name string) ([]byte, error) {
	file, err := d.openRegularFile(name)
	if err != nil || file == nil {
		return nil, err
	}
	defer file.Close()
	content, err := io.ReadAll(file)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", rustPathPush(d.path, name), err)
	}
	if content == nil {
		content = []byte{}
	}
	return content, nil
}

// ReadToString reads text without following links or opening a non-regular
// file for reading. Returns nil when the name does not exist.
func (d *ConfinedDir) ReadToString(name string) (*string, error) {
	content, err := d.ReadBytes(name)
	if err != nil || content == nil {
		return nil, err
	}
	// Rust's File::read_to_string fails with io::Error InvalidData, not
	// String::from_utf8's Utf8Error.
	if !utf8.Valid(content) {
		return nil, fmt.Errorf("read %s: stream did not contain valid UTF-8", rustPathPush(d.path, name))
	}
	text := string(content)
	return &text, nil
}

// WriteAtomically publishes `body` as the file `name` in this directory
// through a temporary file in this directory, so a reader sees the old file
// or the complete new one. A new file is readable by its owner only; a
// replaced file keeps its permissions. Refuses a `name` that is not a
// regular file, such as a link, rather than replacing it.
func (d *ConfinedDir) WriteAtomically(name string, body []byte, publish Publish) PublishError {
	length := uint64(len(body))
	_, err := d.WriteAtomicallyFromReader(name, bytes.NewReader(body), &length, publish)
	return err
}

// WriteAtomicallyFromReader streams `reader` into a temporary file in this
// directory in 64 KiB chunks and atomically publishes it as `name`. When
// `expectedLen` is set, rejects both short reads and trailing extra bytes
// before syncing or publishing, removing the temporary file on any error.
func (d *ConfinedDir) WriteAtomicallyFromReader(name string, reader io.Reader, expectedLen *uint64, publish Publish) (uint64, PublishError) {
	path := rustPathPush(d.path, name)
	var existing *fs.FileMode
	mode, err := lstatAt(d.dir, name)
	switch {
	case err == nil && !mode.IsRegular():
		return 0, PublishErrorNotARegularFile{}
	case err == nil && publish == PublishCreateNew:
		return 0, PublishErrorAlreadyExists{}
	case err == nil:
		permissions := mode.Perm()
		existing = &permissions
	case errors.Is(err, fs.ErrNotExist):
	default:
		return 0, PublishErrorFailed{fmt.Errorf("inspect %s: %w", path, err)}
	}
	var random [8]byte
	if _, err := rand.Read(random[:]); err != nil {
		return 0, PublishErrorFailed{fmt.Errorf("generate temporary filename: %w", err)}
	}
	temporary := fmt.Sprintf(".%s.%d.%016x.tmp", name, os.Getpid(), binary.LittleEndian.Uint64(random[:]))
	written, result := d.publish(name, temporary, path, reader, expectedLen, publish, existing)
	if result != nil {
		_ = removeAt(d.dir, temporary, false)
		return 0, result
	}
	return written, nil
}

func (d *ConfinedDir) publish(name, temporary, path string, reader io.Reader, expectedLen *uint64, publish Publish, existing *fs.FileMode) (uint64, PublishError) {
	file, err := createFileNoFollow(d.dir, temporary)
	if err != nil {
		return 0, PublishErrorFailed{fmt.Errorf("create a temporary file for %s: %w", path, err)}
	}
	var written uint64
	failure := func() PublishError {
		defer file.Close()
		if existing != nil {
			if err := file.Chmod(*existing); err != nil {
				return PublishErrorFailed{fmt.Errorf("keep the permissions of %s: %w", path, err)}
			}
		} else if err := restrictNewFile(file); err != nil {
			return PublishErrorFailed{fmt.Errorf("restrict %s: %w", path, err)}
		}
		buffer := make([]byte, 64*1024)
		for {
			count, err := reader.Read(buffer)
			if count > 0 {
				next := written + uint64(count)
				if next < written {
					next = math.MaxUint64
				}
				if expectedLen != nil && next > *expectedLen {
					return PublishErrorLengthMismatch{Expected: *expectedLen, Received: next}
				}
				if _, err := file.Write(buffer[:count]); err != nil {
					return PublishErrorFailed{fmt.Errorf("write %s: %w", path, err)}
				}
				written = next
			}
			if errors.Is(err, io.EOF) {
				break
			}
			if err != nil {
				return PublishErrorFailed{fmt.Errorf("read input for %s: %w", path, err)}
			}
		}
		if expectedLen != nil && written != *expectedLen {
			return PublishErrorLengthMismatch{Expected: *expectedLen, Received: written}
		}
		if err := file.Sync(); err != nil {
			return PublishErrorFailed{fmt.Errorf("sync %s: %w", path, err)}
		}
		return nil
	}()
	if failure != nil {
		return 0, failure
	}
	switch publish {
	case PublishReplace:
		// A rename replaces whatever entry holds `name` at that moment, a
		// link included, and never follows it.
		if err := renameAt(d.dir, temporary, name); err != nil {
			return 0, PublishErrorFailed{fmt.Errorf("replace %s: %w", path, err)}
		}
	case PublishCreateNew:
		// A link fails when `name` exists, so a file that appeared since
		// the check above is never replaced.
		if err := linkAt(d.dir, temporary, name); err != nil {
			if errors.Is(err, fs.ErrExist) {
				return 0, PublishErrorAlreadyExists{}
			}
			return 0, PublishErrorFailed{fmt.Errorf("publish %s: %w", path, err)}
		}
		if err := removeAt(d.dir, temporary, false); err != nil {
			return 0, PublishErrorFailed{fmt.Errorf("remove the temporary file for %s: %w", path, err)}
		}
	}
	if err := d.sync(); err != nil {
		return 0, PublishErrorFailed{err}
	}
	return written, nil
}

// RemoveFile removes the file or link `name` in this directory; a link is
// removed itself, never what it points to.
func (d *ConfinedDir) RemoveFile(name string) error {
	return removeAt(d.dir, name, false)
}

// RemoveDir removes the empty directory `name` in this directory.
func (d *ConfinedDir) RemoveDir(name string) error {
	return removeAt(d.dir, name, true)
}

func (d *ConfinedDir) tryClone() (*ConfinedDir, error) {
	dir, err := duplicateDir(d.dir, d.path)
	if err != nil {
		return nil, fmt.Errorf("duplicate the handle of %s: %w", d.path, err)
	}
	return &ConfinedDir{dir: dir, path: d.path}, nil
}

// sync makes a rename or new entry in this directory durable. Directory
// fsync is only available on Unix, as in SyncDirectory.
func (d *ConfinedDir) sync() error {
	if err := syncDir(d.dir, d.path); err != nil {
		return fmt.Errorf("sync directory %s: %w", d.path, err)
	}
	return nil
}

func splitRoot(root string) (string, string, error) {
	name, ok := pathFileName(root)
	if !ok {
		return "", "", fmt.Errorf("%s does not end in a directory name", root)
	}
	parent, ok := pathParent(root)
	if !ok || parent == "" {
		parent = "."
	}
	return parent, name, nil
}

func normalComponents(relative string) ([]string, error) {
	var names []string
	for _, component := range pathComponents(relative) {
		if component.kind != pathComponentNormal {
			return nil, fmt.Errorf("%s is not a plain relative path below a confined directory", relative)
		}
		names = append(names, component.text)
	}
	return names, nil
}

func createAndOpen(parent *os.File, name string, path string) (*os.File, error) {
	if err := mkdirAt(parent, name); err != nil && !errors.Is(err, fs.ErrExist) {
		return nil, fmt.Errorf("create %s: %w", path, err)
	}
	dir, err := openDirNoFollow(parent, name)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", noFollowDirectory(path), err)
	}
	return dir, nil
}

func noFollowDirectory(path string) string {
	return fmt.Sprintf("open %s as a directory without following a link", path)
}
