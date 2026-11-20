// Port of mj-checkpoint/src/resources.rs: the parts for every platform.
//
// Streaming snapshots of directories attached to remote session targets.
// StreamResource is in resources_unix.go and resources_other.go.

package checkpoint

import (
	"errors"
	"fmt"
	"io"
	"os"
)

// maxEntries is the most entries a resource archive may hold.
const maxEntries = 1_000_000

// maxExpandedBytes is the most bytes a resource archive may expand to.
const maxExpandedBytes uint64 = 50 * 1024 * 1024 * 1024

// ensureEntryLimit refuses another entry after maxEntries.
func ensureEntryLimit(entries int) error {
	if entries >= maxEntries {
		return errors.New("resource archive has too many entries")
	}
	return nil
}

// InstallResourceStream unpacks a resource tar.gz stream into destination,
// accepting only regular files and directories at safe relative paths,
// never writing through a symbolic link, and refusing more than maxEntries
// entries or maxExpandedBytes bytes.
func InstallResourceStream(input io.Reader, destination string) error {
	panic("unported: mj-checkpoint/src/resources.rs install_resource_stream")
}

// ensureNotSymlink refuses a destination path that is a symbolic link.
func ensureNotSymlink(path string) error {
	if info, err := os.Lstat(path); err == nil && info.Mode()&os.ModeSymlink != 0 {
		return fmt.Errorf("resource destination is a symbolic link: %s", path)
	}
	return nil
}

// validateDestination refuses a destination that is relative, the
// filesystem root, or holds a `..` component.
func validateDestination(destination string) error {
	panic("unported: mj-checkpoint/src/resources.rs validate_destination")
}

// ensureSafeParent refuses a parent below root reached through a symbolic
// link.
func ensureSafeParent(root, parent string) error {
	panic("unported: mj-checkpoint/src/resources.rs ensure_safe_parent")
}
