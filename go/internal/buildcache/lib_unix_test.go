//go:build unix

// Unix file identity for the crate's tests (Rust: std::os::unix::fs::MetadataExt).

package buildcache

import (
	"os"
	"syscall"
	"testing"
)

// inode is the file's inode number (Rust: MetadataExt::ino).
func inode(t *testing.T, path string) uint64 {
	t.Helper()
	_, ino := deviceAndInode(t, path)
	return ino
}

// deviceAndInode is the file's device and inode (Rust: MetadataExt::dev and
// ino).
func deviceAndInode(t *testing.T, path string) (uint64, uint64) {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	stat := info.Sys().(*syscall.Stat_t)
	return uint64(stat.Dev), uint64(stat.Ino)
}
