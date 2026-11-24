// Port of mj-core/src/file_lock.rs.
//
// File locks whose release belongs to their owner, even across fork.

package core

import (
	"errors"
	"log/slog"
	"os"
	"time"
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

// TryLockFile is Rust's File::try_lock (shared false) or
// File::try_lock_shared (shared true): the lock without waiting, false when
// another holder excludes it (TryLockError::WouldBlock). A successful caller
// hands file to NewFileLock.
func TryLockFile(file *os.File, shared bool) (bool, error) {
	if shared {
		return tryLockFileShared(file)
	}
	return tryLockFile(file)
}

// AcquireFileLockCancellable waits for an exclusive lock on file, checking
// cancellation between attempts (Rust FileLock::acquire_cancellable).
func AcquireFileLockCancellable(file *os.File, cancelled func() bool) (*FileLock, error) {
	for {
		if cancelled() {
			return nil, errors.New("file lock wait cancelled")
		}
		locked, err := tryLockFile(file)
		if err != nil {
			return nil, err
		}
		if locked {
			return NewFileLock(file), nil
		}
		time.Sleep(25 * time.Millisecond)
	}
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
