// Port of mj-worker/src/worker_runtime/root_owner.rs: Rust's File::try_lock,
// try_lock_shared and unlock on Windows (LockFileEx and UnlockFileEx over the
// whole file).

//go:build windows

package worker

import (
	"errors"
	"os"

	"golang.org/x/sys/windows"
)

// rootOwnerTryLock takes file's lock without waiting; false when another
// owner holds a conflicting one.
func rootOwnerTryLock(file *os.File, shared bool) (bool, error) {
	flags := uint32(windows.LOCKFILE_FAIL_IMMEDIATELY)
	if !shared {
		flags |= windows.LOCKFILE_EXCLUSIVE_LOCK
	}
	var overlapped windows.Overlapped
	err := windows.LockFileEx(windows.Handle(file.Fd()), flags, 0, ^uint32(0), ^uint32(0), &overlapped)
	if errors.Is(err, windows.ERROR_LOCK_VIOLATION) {
		return false, nil
	}
	return err == nil, err
}

// rootOwnerUnlock is Rust's File::unlock.
func rootOwnerUnlock(file *os.File) error {
	return windows.UnlockFileEx(windows.Handle(file.Fd()), 0, ^uint32(0), ^uint32(0), new(windows.Overlapped))
}
