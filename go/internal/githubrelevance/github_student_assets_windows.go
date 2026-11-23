//go:build windows

// Port of mj-github-relevance/src/github_student/assets.rs: the Windows half
// of the publication lock (Rust's File::lock, LockFileEx).

package githubrelevance

import (
	"os"

	"golang.org/x/sys/windows"
)

// lockPublication waits for the exclusive lock on file.
func lockPublication(file *os.File) error {
	var overlapped windows.Overlapped
	return windows.LockFileEx(windows.Handle(file.Fd()), windows.LOCKFILE_EXCLUSIVE_LOCK, 0, ^uint32(0), ^uint32(0), &overlapped)
}
