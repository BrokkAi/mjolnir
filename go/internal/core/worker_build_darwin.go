// Port of mj-core/src/worker_build.rs (ExecutableIdentity on macOS).

//go:build darwin

package core

import (
	"os"
	"syscall"
)

// executableMemoSupported: Unix identifies a file version by device, inode,
// size and both timestamps, so reads can be memoized.
const executableMemoSupported = true

func executableIdentityFromMetadata(info os.FileInfo) ExecutableIdentity {
	stat := info.Sys().(*syscall.Stat_t)
	return ExecutableIdentity{
		device:   uint64(stat.Dev),
		inode:    uint64(stat.Ino),
		size:     uint64(info.Size()),
		modified: [2]int64{int64(stat.Mtimespec.Sec), int64(stat.Mtimespec.Nsec)},
		changed:  [2]int64{int64(stat.Ctimespec.Sec), int64(stat.Ctimespec.Nsec)},
	}
}
