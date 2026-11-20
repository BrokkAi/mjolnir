// Port of the non-Linux, non-macOS branches of
// rename_directory_without_replacement in mj-worker/src/checkpoint.rs.

//go:build !linux && !darwin

package worker

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"runtime"
)

// renameDirectoryWithoutReplacement publishes source at destination unless
// something already exists there; false when it does. Windows refuses
// replacement of directories, including empty ones; other Unix hosts have no
// atomic no-replace rename, so publication is refused there.
func renameDirectoryWithoutReplacement(source, destination string) (bool, error) {
	if runtime.GOOS != "windows" {
		return false, errors.New("atomic checkout publication is unsupported on this host")
	}
	err := os.Rename(source, destination)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, fs.ErrExist) {
		return false, nil
	}
	return false, fmt.Errorf("publish recovered checkout: %w", err)
}
