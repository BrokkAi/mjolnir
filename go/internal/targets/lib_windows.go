//go:build windows

// File locks the package's modules take on Windows (Rust's File::lock and
// File::try_lock, LockFileEx over the whole file).

package targets

import (
	"errors"
	"os"

	"golang.org/x/sys/windows"
)

// libLockFile waits for an exclusive lock on file.
func libLockFile(file *os.File) error {
	return windows.LockFileEx(windows.Handle(file.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK, 0, ^uint32(0), ^uint32(0), new(windows.Overlapped))
}

// libTryLockFile takes an exclusive lock on file without waiting.
// False when another handle holds it.
func libTryLockFile(file *os.File) (bool, error) {
	err := windows.LockFileEx(windows.Handle(file.Fd()),
		windows.LOCKFILE_EXCLUSIVE_LOCK|windows.LOCKFILE_FAIL_IMMEDIATELY, 0, ^uint32(0), ^uint32(0), new(windows.Overlapped))
	switch {
	case err == nil:
		return true, nil
	case errors.Is(err, windows.ERROR_LOCK_VIOLATION):
		return false, nil
	}
	return false, err
}
