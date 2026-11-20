// Port of the `cfg(not(unix))` parts of mj-checkpoint/src/checkpoint.rs and
// its submodules.

//go:build !unix

package checkpoint

import "io/fs"

// checkpointNoFollow is zero: Rust sets O_NOFOLLOW only on Unix.
const checkpointNoFollow = 0

// checkpointRawMode has no st_mode to report off Unix, where the source
// fingerprint leaves the mode out.
func checkpointRawMode(fs.FileInfo) (uint32, bool) {
	return 0, false
}
