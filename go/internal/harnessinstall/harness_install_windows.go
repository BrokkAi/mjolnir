// Port of mj-harness-install/src/harness_install.rs: Rust's File::try_lock,
// try_lock_shared and lock_shared, which are LockFileEx on Windows.

//go:build windows

package harnessinstall

import (
	"errors"
	"os"

	"golang.org/x/sys/windows"
)

func harnessLockFlags(shared, wait bool) uint32 {
	var flags uint32
	if !shared {
		flags |= windows.LOCKFILE_EXCLUSIVE_LOCK
	}
	if !wait {
		flags |= windows.LOCKFILE_FAIL_IMMEDIATELY
	}
	return flags
}

// harnessTryLock takes file's lock without waiting; locked is false when
// another handle holds a conflicting one.
func harnessTryLock(file *os.File, shared bool) (locked bool, err error) {
	overlapped := new(windows.Overlapped)
	err = windows.LockFileEx(windows.Handle(file.Fd()), harnessLockFlags(shared, false), 0, ^uint32(0), ^uint32(0), overlapped)
	if errors.Is(err, windows.ERROR_LOCK_VIOLATION) {
		return false, nil
	}
	return err == nil, err
}

// harnessRename is Rust's std::fs::rename (MoveFileEx with replacement).
func harnessRename(oldPath, newPath string) error {
	return os.Rename(oldPath, newPath)
}

// harnessLock waits for file's lock.
func harnessLock(file *os.File, shared bool) error {
	overlapped := new(windows.Overlapped)
	return windows.LockFileEx(windows.Handle(file.Fd()), harnessLockFlags(shared, true), 0, ^uint32(0), ^uint32(0), overlapped)
}
