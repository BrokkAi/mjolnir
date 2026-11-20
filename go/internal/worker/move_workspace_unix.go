// Port of mj-worker/src/move_workspace.rs: the Unix capture lock (Rust's
// File::try_lock, flock(LOCK_EX | LOCK_NB)).

//go:build unix

package worker

import (
	"errors"
	"os"
	"syscall"
)

// moveTryLock takes an exclusive lock on file without waiting. It fails with
// Rust's TryLockError::WouldBlock text when another owner holds it.
func moveTryLock(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
		switch {
		case err == nil:
			return nil
		case errors.Is(err, syscall.EINTR):
			continue
		case errors.Is(err, syscall.EWOULDBLOCK):
			return errors.New("try_lock failed because the operation would block")
		}
		return err
	}
}
