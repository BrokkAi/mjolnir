//go:build windows

// Port of the Windows half of mj-checkpoint/src/move_stream.rs's
// open_keep_file_nofollow.

package checkpoint

import (
	"fmt"
	"os"
)

// openKeepFileNofollow creates or truncates a keep file; Windows has no
// mode bits or O_NOFOLLOW to apply.
func openKeepFileNofollow(path string, _ os.FileMode) (*os.File, error) {
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o666)
	if err != nil {
		return nil, fmt.Errorf("create keep file %s: %w", path, err)
	}
	return file, nil
}
