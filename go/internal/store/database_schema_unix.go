// Port of mj-store/src/database/schema.rs (StoreIdentity on Unix).

//go:build unix

package store

import (
	"os"
	"syscall"
)

// storeIdentityOf is the device and inode path names now; false when it
// cannot be read.
func storeIdentityOf(path string) (storeIdentity, bool) {
	info, err := os.Stat(path)
	if err != nil {
		return storeIdentity{}, false
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return storeIdentity{}, false
	}
	return storeIdentity{device: uint64(stat.Dev), inode: uint64(stat.Ino)}, true
}
