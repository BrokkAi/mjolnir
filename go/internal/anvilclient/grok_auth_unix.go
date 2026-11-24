//go:build unix

// Port of brokk-anvil-client 0.28.6 src/grok_auth.rs: the Unix credential
// lock (fs2's try_lock_exclusive is flock(LOCK_EX | LOCK_NB)).

package anvilclient

import (
	"errors"
	"os"
	"path/filepath"
	"syscall"
)

// grokTryLockExclusive takes an exclusive lock on file without waiting.
// False with a nil error when another holder has it.
func grokTryLockExclusive(file *os.File) (bool, error) {
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

// grokLockIsCurrent is whether the held lock file is still the one at the
// lock path, not one replaced while waiting.
func grokLockIsCurrent(file *os.File, authPath string) (bool, error) {
	held, err := file.Stat()
	if err != nil {
		return false, err
	}
	current, err := os.Stat(filepath.Join(filepath.Dir(authPath), "auth.json.lock"))
	if err != nil {
		return false, err
	}
	heldStat, heldOK := held.Sys().(*syscall.Stat_t)
	currentStat, currentOK := current.Sys().(*syscall.Stat_t)
	if !heldOK || !currentOK {
		return os.SameFile(held, current), nil
	}
	return heldStat.Dev == currentStat.Dev && heldStat.Ino == currentStat.Ino, nil
}
