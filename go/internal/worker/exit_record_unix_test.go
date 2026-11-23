// The `cfg(unix)` inode check of mj-worker/src/exit_record.rs's tests.

//go:build unix

package worker

import (
	"os"
	"syscall"
	"testing"
)

// exitRecordInode is path's inode; Unix has one.
func exitRecordInode(t *testing.T, path string) (uint64, bool) {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return uint64(info.Sys().(*syscall.Stat_t).Ino), true
}
