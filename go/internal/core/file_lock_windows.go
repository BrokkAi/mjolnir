//go:build windows

// Port of mj-core/src/file_lock.rs: the Windows unlock (Rust's File::unlock,
// UnlockFile over the whole file).

package core

import (
	"os"

	"golang.org/x/sys/windows"
)

func fileUnlock(file *os.File) error {
	return windows.UnlockFileEx(windows.Handle(file.Fd()), 0, ^uint32(0), ^uint32(0), new(windows.Overlapped))
}
