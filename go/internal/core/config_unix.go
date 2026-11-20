//go:build unix

// Port of mj-core/src/config.rs: the Unix half of ConfigLock (Rust's
// File::lock and File::try_lock, flock(2)).

package core

import (
	"errors"
	"os"
	"syscall"
)

// configLockExclusive waits for the exclusive lock on file.
func configLockExclusive(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
		if !errors.Is(err, syscall.EINTR) {
			return err
		}
	}
}

// configTryLock takes the exclusive lock on file without waiting; false when
// another owner holds it.
func configTryLock(file *os.File) (bool, error) {
	err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
	if errors.Is(err, syscall.EWOULDBLOCK) {
		return false, nil
	}
	return err == nil, err
}
