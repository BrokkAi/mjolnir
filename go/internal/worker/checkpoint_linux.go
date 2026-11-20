// Port of the Linux branch of rename_directory_without_replacement in
// mj-worker/src/checkpoint.rs.

package worker

import (
	"errors"
	"fmt"

	"golang.org/x/sys/unix"
)

// renameDirectoryWithoutReplacement publishes source at destination unless
// something already exists there; false when it does. The kernel makes
// absence and publication one decision, so a child's newly created directory
// can never be replaced by this archive.
func renameDirectoryWithoutReplacement(source, destination string) (bool, error) {
	err := unix.Renameat2(unix.AT_FDCWD, source, unix.AT_FDCWD, destination, unix.RENAME_NOREPLACE)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, unix.EEXIST) {
		return false, nil
	}
	return false, fmt.Errorf("publish recovered checkout without replacing current files: %w", err)
}
