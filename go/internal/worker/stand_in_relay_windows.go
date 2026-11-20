// Port of mj-worker/src/stand_in_relay.rs: the leased relay's journal owner
// lock on Windows (Rust's File::lock, LockFileEx over the whole file).

//go:build testhooks && windows

package worker

import (
	"os"

	"golang.org/x/sys/windows"
)

// standInLockExclusive waits for the exclusive lock on file.
func standInLockExclusive(file *os.File) error {
	overlapped := new(windows.Overlapped)
	return windows.LockFileEx(windows.Handle(file.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK, 0, ^uint32(0), ^uint32(0), overlapped)
}
