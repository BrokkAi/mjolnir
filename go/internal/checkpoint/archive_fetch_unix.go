//go:build unix

// Port of mj-checkpoint/src/archive/fetch.rs: the Unix halves of the fetch
// destination lock (Rust's File::lock, flock(LOCK_EX)) and of the anonymous
// spool file (tempfile::tempfile_in).

package checkpoint

import (
	"os"
	"syscall"
)

// fetchLockExclusive waits for the exclusive lock on file.
func fetchLockExclusive(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
		if err != syscall.EINTR {
			return err
		}
	}
}

// fetchAnonymousFile creates a file in directory that has no name: it is
// unlinked as soon as it is open, so nothing is left behind when it closes.
func fetchAnonymousFile(directory string) (*os.File, error) {
	file, err := os.CreateTemp(directory, ".tmp")
	if err != nil {
		return nil, err
	}
	if err := os.Remove(file.Name()); err != nil {
		file.Close()
		return nil, err
	}
	return file, nil
}
