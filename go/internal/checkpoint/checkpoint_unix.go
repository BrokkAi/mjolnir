// Port of the `cfg(unix)` parts of mj-checkpoint/src/checkpoint.rs and its
// submodules.

//go:build unix

package checkpoint

import (
	"io/fs"
	"syscall"
)

// checkpointNoFollow is O_NOFOLLOW, which the private writers and the stage
// reader open with so a final symlink is refused rather than followed.
const checkpointNoFollow = syscall.O_NOFOLLOW

// checkpointRawMode is the file's whole st_mode, type bits included (Rust's
// MetadataExt::mode), which the source fingerprint hashes.
func checkpointRawMode(info fs.FileInfo) (uint32, bool) {
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return 0, false
	}
	return uint32(stat.Mode), true
}
