//go:build unix

// Port of mj-core/src/credentials/locking.rs: the Unix try-lock (Rust's
// File::try_lock, flock(LOCK_EX | LOCK_NB)).

package core

import (
	"errors"
	"os"
	"syscall"
)

// tryLockFile takes an exclusive lock on file without waiting. False when
// another open file description holds it.
func tryLockFile(file *os.File) (bool, error) {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
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
