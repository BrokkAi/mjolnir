//go:build windows

// Port of mj-core/src/config.rs: the Windows half of ConfigLock (Rust's
// File::lock and File::try_lock, LockFileEx).

package core

import (
	"errors"
	"os"

	"golang.org/x/sys/windows"
)

// configLockExclusive waits for the exclusive lock on file.
func configLockExclusive(file *os.File) error {
	var overlapped windows.Overlapped
	return windows.LockFileEx(windows.Handle(file.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK, 0, ^uint32(0), ^uint32(0), &overlapped)
}

// configTryLock takes the exclusive lock on file without waiting; false when
// another owner holds it.
func configTryLock(file *os.File) (bool, error) {
	var overlapped windows.Overlapped
	err := windows.LockFileEx(windows.Handle(file.Fd()),
		windows.LOCKFILE_EXCLUSIVE_LOCK|windows.LOCKFILE_FAIL_IMMEDIATELY, 0, ^uint32(0), ^uint32(0), &overlapped)
	if errors.Is(err, windows.ERROR_LOCK_VIOLATION) {
		return false, nil
	}
	return err == nil, err
}
