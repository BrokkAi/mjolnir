//go:build windows

// Port of mj-checkpoint/src/archive/fetch.rs: the Windows halves of the
// fetch destination lock (Rust's File::lock, LockFileEx) and of the
// anonymous spool file (tempfile::tempfile_in, which deletes on close).

package checkpoint

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"os"
	"path/filepath"

	"golang.org/x/sys/windows"
)

// fetchLockExclusive waits for the exclusive lock on file.
func fetchLockExclusive(file *os.File) error {
	var overlapped windows.Overlapped
	return windows.LockFileEx(windows.Handle(file.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK, 0, ^uint32(0), ^uint32(0), &overlapped)
}

// fetchAnonymousFile creates a file in directory that Windows deletes when
// its last handle closes, so nothing is left behind.
func fetchAnonymousFile(directory string) (*os.File, error) {
	for {
		var random [8]byte
		if _, err := rand.Read(random[:]); err != nil {
			return nil, err
		}
		path := filepath.Join(directory, ".tmp"+hex.EncodeToString(random[:]))
		name, err := windows.UTF16PtrFromString(path)
		if err != nil {
			return nil, err
		}
		handle, err := windows.CreateFile(name,
			windows.GENERIC_READ|windows.GENERIC_WRITE,
			windows.FILE_SHARE_READ|windows.FILE_SHARE_WRITE|windows.FILE_SHARE_DELETE,
			nil, windows.CREATE_NEW,
			windows.FILE_ATTRIBUTE_TEMPORARY|windows.FILE_FLAG_DELETE_ON_CLOSE, 0)
		if errors.Is(err, windows.ERROR_FILE_EXISTS) {
			continue
		}
		if err != nil {
			return nil, &os.PathError{Op: "open", Path: path, Err: err}
		}
		return os.NewFile(uintptr(handle), path), nil
	}
}
