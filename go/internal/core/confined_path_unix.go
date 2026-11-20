// Port of mj-core/src/confined_path.rs: the handle operations on Unix,
// resolving one name at a time below an open directory with openat-style
// calls and O_NOFOLLOW, as cap-std does in Rust.

//go:build unix

package core

import (
	"io/fs"
	"os"

	"golang.org/x/sys/unix"
)

// pathError keeps the failing call and name in the error, and lets
// errors.Is see fs.ErrNotExist and fs.ErrExist through the errno.
func pathError(op, name string, err error) error {
	return &os.PathError{Op: op, Path: name, Err: err}
}

func openAt(parent *os.File, name string, flags int, mode uint32) (*os.File, error) {
	for {
		fd, err := unix.Openat(int(parent.Fd()), name, flags|unix.O_CLOEXEC, mode)
		if err == unix.EINTR {
			continue
		}
		if err != nil {
			return nil, pathError("openat", name, err)
		}
		return os.NewFile(uintptr(fd), name), nil
	}
}

func openDirNoFollow(parent *os.File, name string) (*os.File, error) {
	return openAt(parent, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW, 0)
}

// openFileNoFollow opens a name for reading without following a link. A FIFO
// must not wait for a writer before its descriptor can be checked.
func openFileNoFollow(parent *os.File, name string) (*os.File, error) {
	return openAt(parent, name, unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_NONBLOCK, 0)
}

// createFileNoFollow creates a new file only readable by its owner until
// the caller sets its permissions.
func createFileNoFollow(parent *os.File, name string) (*os.File, error) {
	return openAt(parent, name, unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW, 0o600)
}

// restrictNewFile makes a new file readable by its owner only.
func restrictNewFile(file *os.File) error { return file.Chmod(0o600) }

// reopenDir is a fresh readable handle on the same directory, with its own
// read offset, for listing or syncing it.
func reopenDir(dir *os.File, path string) (*os.File, error) {
	fd, err := unix.Openat(int(dir.Fd()), ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, pathError("openat", path, err)
	}
	return os.NewFile(uintptr(fd), path), nil
}

func duplicateDir(dir *os.File, path string) (*os.File, error) {
	fd, err := unix.Dup(int(dir.Fd()))
	if err != nil {
		return nil, pathError("dup", path, err)
	}
	unix.CloseOnExec(fd)
	return os.NewFile(uintptr(fd), path), nil
}

// lstatAt is the file type and permissions of `name` without following a
// link.
func lstatAt(dir *os.File, name string) (fs.FileMode, error) {
	var stat unix.Stat_t
	if err := unix.Fstatat(int(dir.Fd()), name, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return 0, pathError("fstatat", name, err)
	}
	mode := fs.FileMode(stat.Mode & 0o777)
	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFDIR:
		mode |= fs.ModeDir
	case unix.S_IFLNK:
		mode |= fs.ModeSymlink
	case unix.S_IFREG:
	default:
		mode |= fs.ModeIrregular
	}
	return mode, nil
}

func mkdirAt(dir *os.File, name string) error {
	if err := unix.Mkdirat(int(dir.Fd()), name, 0o777); err != nil {
		return pathError("mkdirat", name, err)
	}
	return nil
}

func renameAt(dir *os.File, from, to string) error {
	if err := unix.Renameat(int(dir.Fd()), from, int(dir.Fd()), to); err != nil {
		return pathError("renameat", to, err)
	}
	return nil
}

func linkAt(dir *os.File, from, to string) error {
	if err := unix.Linkat(int(dir.Fd()), from, int(dir.Fd()), to, 0); err != nil {
		return pathError("linkat", to, err)
	}
	return nil
}

func removeAt(dir *os.File, name string, directory bool) error {
	flags := 0
	if directory {
		flags = unix.AT_REMOVEDIR
	}
	if err := unix.Unlinkat(int(dir.Fd()), name, flags); err != nil {
		return pathError("unlinkat", name, err)
	}
	return nil
}

// syncDir syncs a readable handle on the same directory.
func syncDir(dir *os.File, path string) error {
	directory, err := reopenDir(dir, path)
	if err != nil {
		return err
	}
	defer directory.Close()
	return directory.Sync()
}
