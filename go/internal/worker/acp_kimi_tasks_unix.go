// Port of mj-worker/src/acp/kimi_tasks.rs: the Unix file identity.

//go:build unix

package worker

import (
	"os"
	"syscall"
)

// kimiFileIdentityKnown: Unix compares a wire stream's device and inode.
const kimiFileIdentityKnown = true

// kimiFileIdentity is the file's device and inode.
func kimiFileIdentity(metadata os.FileInfo) (device, inode uint64) {
	stat, ok := metadata.Sys().(*syscall.Stat_t)
	if !ok {
		return 0, 0
	}
	return uint64(stat.Dev), uint64(stat.Ino)
}
