// Port of mj-harness-install/src/harness_install.rs: Rust's File::try_lock,
// try_lock_shared and lock_shared, which are flock(2) on Unix.

//go:build unix

package core

import (
	"errors"
	"os"
	"syscall"
)

// harnessTryLock takes file's lock without waiting; locked is false when
// another open file description holds a conflicting one.
func harnessTryLock(file *os.File, shared bool) (locked bool, err error) {
	how := syscall.LOCK_EX
	if shared {
		how = syscall.LOCK_SH
	}
	for {
		err = syscall.Flock(int(file.Fd()), how|syscall.LOCK_NB)
		if !errors.Is(err, syscall.EINTR) {
			break
		}
	}
	if errors.Is(err, syscall.EWOULDBLOCK) {
		return false, nil
	}
	return err == nil, err
}

// harnessRename is rename(2), as Rust's std::fs::rename: unlike os.Rename,
// a directory replaces an existing empty directory.
func harnessRename(oldPath, newPath string) error {
	if err := syscall.Rename(oldPath, newPath); err != nil {
		return &os.LinkError{Op: "rename", Old: oldPath, New: newPath, Err: err}
	}
	return nil
}

// harnessLock waits for file's lock.
func harnessLock(file *os.File, shared bool) error {
	how := syscall.LOCK_EX
	if shared {
		how = syscall.LOCK_SH
	}
	for {
		err := syscall.Flock(int(file.Fd()), how)
		if !errors.Is(err, syscall.EINTR) {
			return err
		}
	}
}
