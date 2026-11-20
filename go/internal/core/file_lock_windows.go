//go:build windows

// Port of mj-core/src/file_lock.rs: the Windows unlock (Rust's File::unlock,
// UnlockFile over the whole file).

package core

import "os"

func fileUnlock(file *os.File) error {
	panic("unported: mj-core/src/file_lock.rs FileLock unlock on Windows (needs UnlockFileEx)")
}
