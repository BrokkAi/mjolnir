//go:build windows

// Port of mj-core/src/file_lock.rs: the Windows unlock (Rust's File::unlock,
// UnlockFile over the whole file), and the shared try-lock Rust's std gives
// every File (File::try_lock_shared, LockFileEx with
// LOCKFILE_FAIL_IMMEDIATELY and no exclusive flag).

package core

import (
	"errors"
	"os"

	"golang.org/x/sys/windows"
)

func fileUnlock(file *os.File) error {
	return windows.UnlockFileEx(windows.Handle(file.Fd()), 0, ^uint32(0), ^uint32(0), new(windows.Overlapped))
}

// tryLockFileShared takes a shared lock on file without waiting. False when
// another handle holds it exclusively.
func tryLockFileShared(file *os.File) (bool, error) {
	overlapped := new(windows.Overlapped)
	err := windows.LockFileEx(windows.Handle(file.Fd()),
		windows.LOCKFILE_FAIL_IMMEDIATELY, 0, ^uint32(0), ^uint32(0), overlapped)
	switch {
	case err == nil:
		return true, nil
	case errors.Is(err, windows.ERROR_LOCK_VIOLATION):
		return false, nil
	}
	return false, err
}
