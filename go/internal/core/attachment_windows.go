//go:build windows

// Port of mj-core/src/attachment.rs: the Windows store lock (Rust's
// File::lock, LockFileEx over the whole file).

package core

import (
	"os"

	"golang.org/x/sys/windows"
)

func attachmentLockFile(file *os.File) error {
	overlapped := new(windows.Overlapped)
	return windows.LockFileEx(windows.Handle(file.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK, 0, ^uint32(0), ^uint32(0), overlapped)
}
