//go:build windows

// Port of brokk-anvil-client 0.28.6 src/grok_auth.rs: the Windows credential
// lock (fs2's try_lock_exclusive is LockFileEx with
// LOCKFILE_FAIL_IMMEDIATELY).

package anvilclient

import (
	"errors"
	"os"

	"golang.org/x/sys/windows"
)

// grokTryLockExclusive takes an exclusive lock on file without waiting.
// False with a nil error when another holder has it.
func grokTryLockExclusive(file *os.File) (bool, error) {
	overlapped := new(windows.Overlapped)
	err := windows.LockFileEx(windows.Handle(file.Fd()),
		windows.LOCKFILE_EXCLUSIVE_LOCK|windows.LOCKFILE_FAIL_IMMEDIATELY, 0, ^uint32(0), ^uint32(0), overlapped)
	switch {
	case err == nil:
		return true, nil
	case errors.Is(err, windows.ERROR_LOCK_VIOLATION):
		return false, nil
	}
	return false, err
}

// grokLockIsCurrent: Rust checks the held lock's identity on Unix only.
func grokLockIsCurrent(*os.File, string) (bool, error) { return true, nil }
