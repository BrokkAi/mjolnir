//go:build windows

// Port of the Windows half of mj-checkpoint/src/snapshot.rs's
// hash_file_metadata.

package checkpoint

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"syscall"
)

// hashFileIdentity hashes the file's creation time as Rust's Debug prints a
// Windows SystemTime. Stable Rust has no Windows file-index accessor. Git's
// lockfile rename gives each occurrence a new creation timestamp with 100 ns
// precision.
func hashFileIdentity(identity io.Writer, metadata fs.FileInfo) error {
	attributes, ok := metadata.Sys().(*syscall.Win32FileAttributeData)
	if !ok {
		return errors.New("file metadata has no creation time")
	}
	intervals := int64(attributes.CreationTime.HighDateTime)<<32 | int64(attributes.CreationTime.LowDateTime)
	fmt.Fprintf(identity, "SystemTime { intervals: %d }", intervals)
	return nil
}
