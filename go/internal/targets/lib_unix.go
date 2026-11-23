//go:build unix

// File locks the package's modules take on Unix (Rust's File::lock and
// File::try_lock, flock).

package targets

import (
	"errors"
	"os"
	"syscall"
)

// libLockFile waits for an exclusive lock on file.
func libLockFile(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
		if errors.Is(err, syscall.EINTR) {
			continue
		}
		return err
	}
}

// libTryLockFile takes an exclusive lock on file without waiting.
// False when another open file description holds it.
func libTryLockFile(file *os.File) (bool, error) {
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
