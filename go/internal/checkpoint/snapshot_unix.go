//go:build !windows

// Port of the Unix half of mj-checkpoint/src/snapshot.rs's
// hash_file_metadata.

package checkpoint

import (
	"errors"
	"io"
	"io/fs"
	"syscall"
)

// hashFileIdentity hashes the file's device and inode numbers.
func hashFileIdentity(identity io.Writer, metadata fs.FileInfo) error {
	stat, ok := metadata.Sys().(*syscall.Stat_t)
	if !ok {
		return errors.New("file metadata has no device and inode numbers")
	}
	identity.Write(snapshotBigEndian64(uint64(stat.Dev)))
	identity.Write(snapshotBigEndian64(uint64(stat.Ino)))
	return nil
}
