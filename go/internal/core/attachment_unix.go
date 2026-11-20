//go:build unix

// Port of mj-core/src/attachment.rs: the Unix store lock (Rust's
// File::lock, flock(LOCK_EX)).

package core

import (
	"os"
	"syscall"
)

func attachmentLockFile(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
		if err != syscall.EINTR {
			return err
		}
	}
}
