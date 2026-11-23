// Port of mj-worker/src/move_workspace.rs: the Windows capture lock (Rust's
// File::try_lock, LockFileEx with LOCKFILE_FAIL_IMMEDIATELY over the whole
// file).

//go:build windows

package worker

import (
	"errors"
	"os"

	"golang.org/x/sys/windows"
)

// moveTryLock takes an exclusive lock on file without waiting. It fails with
// Rust's TryLockError::WouldBlock text when another handle holds it.
func moveTryLock(file *os.File) error {
	overlapped := new(windows.Overlapped)
	err := windows.LockFileEx(windows.Handle(file.Fd()),
		windows.LOCKFILE_EXCLUSIVE_LOCK|windows.LOCKFILE_FAIL_IMMEDIATELY, 0, ^uint32(0), ^uint32(0), overlapped)
	if errors.Is(err, windows.ERROR_LOCK_VIOLATION) {
		return errors.New("try_lock failed because the operation would block")
	}
	return err
}
