// Port of mj-worker/src/authorized_keys.rs: the Unix modes and `flock`.

//go:build unix

package worker

import (
	"errors"
	"fmt"
	"os"
	"syscall"
)

func chmodSSHDir(dir string) error {
	if err := os.Chmod(dir, 0o700); err != nil {
		return fmt.Errorf("chmod 0700 %s: %w", dir, err)
	}
	return nil
}

func setFileMode0600(file *os.File) error {
	if err := file.Chmod(0o600); err != nil {
		return fmt.Errorf("chmod 0600 %s: %w", file.Name(), err)
	}
	return nil
}

// authorizedKeysLockExclusive waits for the exclusive `flock`, retrying when
// a signal interrupts the wait.
func authorizedKeysLockExclusive(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
		if !errors.Is(err, syscall.EINTR) {
			return err
		}
	}
}

func authorizedKeysUnlock(file *os.File) {
	_ = syscall.Flock(int(file.Fd()), syscall.LOCK_UN)
}
