// Port of mj-core/src/confined_path.rs: the handle operations on Windows.
//
// Windows has no openat. Each operation resolves the name against the
// directory handle's path and refuses a link or junction it finds there
// (Lstat sees reparse points as links), which keeps reads and writes from
// following one. A Windows controller runs no local sessions, so nothing a
// session's agent controls writes below these directories.

//go:build windows

package core

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
)

var errConfinedLink = errors.New("refusing to follow a link")

func confinedName(dir *os.File, name string) string { return filepath.Join(dir.Name(), name) }

func openDirNoFollow(parent *os.File, name string) (*os.File, error) {
	path := confinedName(parent, name)
	info, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}
	if info.Mode()&fs.ModeSymlink != 0 || info.Mode()&fs.ModeIrregular != 0 {
		return nil, &os.PathError{Op: "open", Path: path, Err: errConfinedLink}
	}
	if !info.IsDir() {
		return nil, &os.PathError{Op: "open", Path: path, Err: fmt.Errorf("not a directory")}
	}
	return os.Open(path)
}

func openFileNoFollow(parent *os.File, name string) (*os.File, error) {
	path := confinedName(parent, name)
	info, err := os.Lstat(path)
	if err != nil {
		return nil, err
	}
	if info.Mode()&fs.ModeSymlink != 0 {
		return nil, &os.PathError{Op: "open", Path: path, Err: errConfinedLink}
	}
	return os.Open(path)
}

func createFileNoFollow(parent *os.File, name string) (*os.File, error) {
	return os.OpenFile(confinedName(parent, name), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
}

// restrictNewFile is a no-op: Windows files take their access from the
// directory's ACL, as in Rust.
func restrictNewFile(*os.File) error { return nil }

func reopenDir(dir *os.File, _ string) (*os.File, error) { return os.Open(dir.Name()) }

func duplicateDir(dir *os.File, _ string) (*os.File, error) { return os.Open(dir.Name()) }

func lstatAt(dir *os.File, name string) (fs.FileMode, error) {
	info, err := os.Lstat(confinedName(dir, name))
	if err != nil {
		return 0, err
	}
	return info.Mode(), nil
}

func mkdirAt(dir *os.File, name string) error { return os.Mkdir(confinedName(dir, name), 0o777) }

func renameAt(dir *os.File, from, to string) error {
	return os.Rename(confinedName(dir, from), confinedName(dir, to))
}

func linkAt(dir *os.File, from, to string) error {
	return os.Link(confinedName(dir, from), confinedName(dir, to))
}

func removeAt(dir *os.File, name string, directory bool) error {
	path := confinedName(dir, name)
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if directory != info.IsDir() && info.Mode()&fs.ModeSymlink == 0 {
		if directory {
			return &os.PathError{Op: "remove", Path: path, Err: fmt.Errorf("not a directory")}
		}
		return &os.PathError{Op: "remove", Path: path, Err: fmt.Errorf("is a directory")}
	}
	return os.Remove(path)
}

// syncDir is a no-op: directory fsync is only available on Unix.
func syncDir(*os.File, string) error { return nil }
