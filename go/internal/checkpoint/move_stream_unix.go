//go:build !windows

// Port of the Unix half of mj-checkpoint/src/move_stream.rs's
// open_keep_file_nofollow.

package checkpoint

import (
	"fmt"
	"os"
	"syscall"
)

// openKeepFileNofollow creates or truncates a keep file with mode, refusing
// to follow a symlink at its final component.
func openKeepFileNofollow(path string, mode os.FileMode) (*os.File, error) {
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC|syscall.O_NOFOLLOW, mode&0o777)
	if err != nil {
		return nil, fmt.Errorf("create keep file %s: %w", path, err)
	}
	return file, nil
}
