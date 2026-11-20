//go:build unix

// Port of mj-core/src/targets/ssh.rs: the Unix file lock of a master's
// check-and-open step (Rust's File::try_lock, flock(LOCK_EX|LOCK_NB)).

package core

import (
	"errors"
	"os"
	"syscall"
)

// sshTryLockFile takes an exclusive lock on file without waiting. false
// means another open file description holds it.
func sshTryLockFile(file *os.File) (bool, error) {
	err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
	if errors.Is(err, syscall.EWOULDBLOCK) {
		return false, nil
	}
	return err == nil, err
}
