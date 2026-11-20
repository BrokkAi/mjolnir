// Port of mj-worker/src/stand_in_relay.rs: the leased relay's journal owner
// lock on Unix (Rust's File::lock, flock(LOCK_EX)).

//go:build testhooks && unix

package worker

import (
	"errors"
	"os"
	"syscall"
)

// standInLockExclusive waits for the exclusive lock on file.
func standInLockExclusive(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
		if !errors.Is(err, syscall.EINTR) {
			return err
		}
	}
}
