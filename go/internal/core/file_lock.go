// Port of mj-core/src/file_lock.rs.
//
// File locks whose release belongs to their owner, even across fork.

package core

import (
	"log/slog"
	"os"
)

// FileLock owns an already locked file. Explicit unlock releases the open
// file description's lock even if an unrelated fork still holds a
// descriptor.
type FileLock struct {
	file *os.File
}

// NewFileLock transfers ownership immediately after successfully locking file.
func NewFileLock(file *os.File) *FileLock {
	return &FileLock{file: file}
}

// File inspects the locked file without duplicating ownership of its lock.
func (l *FileLock) File() *os.File {
	if l.file == nil {
		panic("file lock has not been released")
	}
	return l.file
}

// Release releases now and reports an unlock failure to the caller.
func (l *FileLock) Release() error {
	file := l.File()
	l.file = nil
	return fileUnlock(file)
}

// Drop is Rust's Drop: release if still held, logging a failure. Call it
// with defer on every path that does not Release.
func (l *FileLock) Drop() {
	if l.file == nil {
		return
	}
	file := l.file
	l.file = nil
	if err := fileUnlock(file); err != nil {
		slog.Warn("could not release file lock", "error", err)
	}
}
