//go:build unix

// Port of mj-core/src/file_lock.rs: the Unix unlock (Rust's File::unlock,
// flock(LOCK_UN)), and the shared try-lock Rust's std gives every File
// (File::try_lock_shared, flock(LOCK_SH | LOCK_NB)).

package core

import (
	"errors"
	"os"
	"syscall"
)

func fileUnlock(file *os.File) error {
	return syscall.Flock(int(file.Fd()), syscall.LOCK_UN)
}

// tryLockFileShared takes a shared lock on file without waiting. False when
// another open file description holds it exclusively.
func tryLockFileShared(file *os.File) (bool, error) {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_SH|syscall.LOCK_NB)
		switch {
		case err == nil:
			return true, nil
		case errors.Is(err, syscall.EINTR):
			continue
		case errors.Is(err, syscall.EWOULDBLOCK):
			return false, nil
		}
		return false, err
	}
}
