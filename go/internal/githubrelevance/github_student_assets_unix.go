//go:build unix

// Port of mj-github-relevance/src/github_student/assets.rs: the Unix half of
// the publication lock (Rust's File::lock, flock(2)).

package githubrelevance

import (
	"errors"
	"os"
	"syscall"
)

// lockPublication waits for the exclusive lock on file.
func lockPublication(file *os.File) error {
	for {
		err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
		if !errors.Is(err, syscall.EINTR) {
			return err
		}
	}
}
