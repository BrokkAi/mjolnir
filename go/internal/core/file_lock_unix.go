//go:build unix

// Port of mj-core/src/file_lock.rs: the Unix unlock (Rust's File::unlock,
// flock(LOCK_UN)).

package core

import (
	"os"
	"syscall"
)

func fileUnlock(file *os.File) error {
	return syscall.Flock(int(file.Fd()), syscall.LOCK_UN)
}
