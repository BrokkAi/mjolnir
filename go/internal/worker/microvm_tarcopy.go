//go:build linux

// Port of mj-worker/src/microvm/tarcopy.rs.
//
// `cp` semantics on tar streams, shared by the host engine and the guest
// agent.
//
// A copy packs one file or directory under its own name. The receiver
// decides where that name lands, the way `podman cp` does: an existing
// directory receives the item inside it; otherwise the destination path
// becomes the item's new name and its parent must exist.
//
// Either side may be unpacking a stream written by the other, and the guest
// is the less trusted one, so unpacking accepts only regular files,
// directories and symlinks, never leaves the destination, and never writes
// through a symlink.

package worker

import (
	"errors"
	"io"
	"os"
	"strings"

	"golang.org/x/sys/unix"
)

// MicrovmCopyMode is what a copy moves.
type MicrovmCopyMode int

const (
	// MicrovmCopyModeItem copies the item itself, under its own name
	// (`cp SRC DEST`).
	MicrovmCopyModeItem MicrovmCopyMode = iota
	// MicrovmCopyModeContents copies a directory's contents
	// (`cp SRC/. DEST`): merged into an existing DEST, which keeps its own
	// metadata, or into a new DEST that takes the source directory's.
	MicrovmCopyModeContents
)

// MicrovmSplitContentsOperand splits Podman's contents form off a `cp`
// operand: `dir/.` and `.` mean "the contents of the directory". Returns
// the directory and the mode.
func MicrovmSplitContentsOperand(operand string) (string, MicrovmCopyMode) {
	if operand == "." {
		return ".", MicrovmCopyModeContents
	}
	if directory, ok := strings.CutSuffix(operand, "/."); ok {
		if directory == "" {
			directory = "/"
		}
		return directory, MicrovmCopyModeContents
	}
	return operand, MicrovmCopyModeItem
}

// MicrovmTarcopyPack writes source as a tar stream to out. In
// MicrovmCopyModeItem entries are named from its final path component; in
// MicrovmCopyModeContents they are named relative to the directory, which
// is itself the first entry, `.`.
func MicrovmTarcopyPack(source string, out io.Writer, mode MicrovmCopyMode) error {
	panic("unported: mj-worker/src/microvm/tarcopy.rs pack")
}

// MicrovmTarcopyUnpack unpacks a tar stream produced by MicrovmTarcopyPack
// at destination.
//
// owner is the owner every created entry receives; nil leaves entries owned
// by the unpacking process.
//
// Every operation is relative to a directory descriptor this function
// holds, and no step follows a symlink, so another process that swaps a
// checked directory for a symlink while the engine runs as root cannot
// redirect the unpacking: the swap is seen as an error, not followed.
func MicrovmTarcopyUnpack(reader io.Reader, destination string, owner *MicrovmOwner, copyMode MicrovmCopyMode) error {
	panic("unported: mj-worker/src/microvm/tarcopy.rs unpack")
}

// microvmTarcopyApplyMetadata gives an entry exactly the mode, owner and
// modification time (seconds, nil to leave it) it carried.
func microvmTarcopyApplyMetadata(fd *os.File, mode uint32, owner *MicrovmOwner, mtime *uint64, label string) error {
	panic("unported: mj-worker/src/microvm/tarcopy.rs apply_metadata")
}

// microvmTarcopyItemDestination is where an item copy lands: inside an
// existing directory (rename nil), or as a new name in an existing parent.
func microvmTarcopyItemDestination(destination string) (root string, rename *string, err error) {
	panic("unported: mj-worker/src/microvm/tarcopy.rs item_destination")
}

// microvmTarcopyContentsDestination opens the directory a contents copy
// fills and reports whether this call created it. A missing destination is
// created in its existing parent; an existing non-directory is refused.
func microvmTarcopyContentsDestination(destination string) (*os.File, bool, error) {
	panic("unported: mj-worker/src/microvm/tarcopy.rs contents_destination")
}

// microvmTarcopyContentsRelative is an entry path of a contents copy,
// without its leading `./`. Empty for the directory itself.
func microvmTarcopyContentsRelative(path string) (string, error) {
	panic("unported: mj-worker/src/microvm/tarcopy.rs contents_relative")
}

// microvmTarcopyRenameTop is an entry path of an item copy with its first
// component replaced by rename, when given.
func microvmTarcopyRenameTop(path string, rename *string) (string, error) {
	panic("unported: mj-worker/src/microvm/tarcopy.rs rename_top")
}

// microvmTarcopyOpenPath is `openat` that reports errno; dir is
// unix.AT_FDCWD or an open directory's descriptor.
func microvmTarcopyOpenPath(dir int, path string, flags int, mode uint32) (*os.File, error) {
	fd, err := unix.Openat(dir, path, flags, mode)
	if err != nil {
		return nil, err
	}
	return os.NewFile(uintptr(fd), path), nil
}

// microvmTarcopyStatAt returns the file type bits of name in dir; ok is
// false when it does not exist. A symlink is not followed.
func microvmTarcopyStatAt(dir *os.File, name string) (mode uint32, ok bool, err error) {
	panic("unported: mj-worker/src/microvm/tarcopy.rs stat_at")
}

func microvmTarcopyUnlinkAt(dir *os.File, name string, flags int) error {
	panic("unported: mj-worker/src/microvm/tarcopy.rs unlink_at")
}

func microvmTarcopyMakeDirAt(dir *os.File, name string) error {
	panic("unported: mj-worker/src/microvm/tarcopy.rs make_dir_at")
}

// microvmTarcopyOpenParents opens (creating missing ones) the directories
// of relative below root, one component at a time, each relative to the
// previous descriptor and refusing symlinks.
func microvmTarcopyOpenParents(root *os.File, relative string) (*os.File, error) {
	panic("unported: mj-worker/src/microvm/tarcopy.rs open_parents")
}

func microvmTarcopyIsErrno(err error, errno unix.Errno) bool {
	return errors.Is(err, errno)
}
