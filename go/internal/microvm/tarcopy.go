//go:build linux

// Port of mj-microvm/src/tarcopy.rs.
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
//
// The tar stream is the one Rust's `tar` crate writes: GNU headers (long
// names as `././@LongLink` entries), every header carrying the file's full
// `st_mode`, owner and modification time in seconds, and the directory of
// a copy named with a trailing slash. Unlike the crate, files with holes
// are written whole: Go's archive/tar writes no sparse entries, which the
// unpacker refuses anyway.

package microvm

import (
	"archive/tar"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"syscall"
	"time"
	"unsafe"

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

// microvmComponentKind is a kind of Rust's std::path::Component.
type microvmComponentKind int

const (
	microvmComponentRootDir microvmComponentKind = iota
	microvmComponentCurDir
	microvmComponentParentDir
	microvmComponentNormal
)

type microvmComponent struct {
	kind microvmComponentKind
	name string
}

// text is the component as Rust's Component::as_os_str gives it.
func (c microvmComponent) text() string {
	switch c.kind {
	case microvmComponentRootDir:
		return "/"
	case microvmComponentCurDir:
		return "."
	case microvmComponentParentDir:
		return ".."
	}
	return c.name
}

// microvmPathComponents is Rust's Path::components on Unix: repeated and
// trailing slashes vanish, and `.` is kept only at the start of a relative
// path.
func microvmPathComponents(path string) []microvmComponent {
	var components []microvmComponent
	absolute := strings.HasPrefix(path, "/")
	if absolute {
		components = append(components, microvmComponent{kind: microvmComponentRootDir})
	}
	for i, part := range strings.Split(path, "/") {
		switch part {
		case "":
		case ".":
			if i == 0 && !absolute {
				components = append(components, microvmComponent{kind: microvmComponentCurDir})
			}
		case "..":
			components = append(components, microvmComponent{kind: microvmComponentParentDir})
		default:
			components = append(components, microvmComponent{kind: microvmComponentNormal, name: part})
		}
	}
	return components
}

// microvmPathFileName is Rust's Path::file_name: the final component when
// it is a normal name.
func microvmPathFileName(path string) (string, bool) {
	components := microvmPathComponents(path)
	if len(components) == 0 || components[len(components)-1].kind != microvmComponentNormal {
		return "", false
	}
	return components[len(components)-1].name, true
}

// microvmPathParent is Rust's Path::parent for a path whose last component
// is a normal name: the path without it, "" when nothing is left.
func microvmPathParent(path string) string {
	components := microvmPathComponents(path)
	if len(components) <= 1 {
		if len(components) == 1 && components[0].kind == microvmComponentRootDir {
			return "/"
		}
		return ""
	}
	return microvmJoinComponents(components[:len(components)-1])
}

func microvmJoinComponents(components []microvmComponent) string {
	var builder strings.Builder
	needsSlash := false
	for _, component := range components {
		if needsSlash {
			builder.WriteByte('/')
		}
		text := component.text()
		builder.WriteString(text)
		needsSlash = text != "/"
	}
	return builder.String()
}

// microvmTarHeaderPath is the name the tar crate stores for path
// (copy_path_into): entry names must be relative and free of `..`, and a
// link target keeps every component.
func microvmTarHeaderPath(path string, isLink bool) (string, error) {
	components := microvmPathComponents(path)
	var builder strings.Builder
	emitted, needsSlash := false, false
	for _, component := range components {
		if !isLink {
			switch component.kind {
			case microvmComponentRootDir:
				return "", errors.New("paths in archives must be relative")
			case microvmComponentParentDir:
				return "", errors.New("paths in archives must not have `..`")
			case microvmComponentCurDir:
				// Allow "./" as the path.
				if len(components) != 1 {
					continue
				}
			}
		}
		if needsSlash {
			builder.WriteByte('/')
		}
		text := component.text()
		builder.WriteString(text)
		needsSlash = text != "/"
		emitted = true
	}
	if !emitted {
		return "", errors.New("paths in archives must have at least one component")
	}
	if strings.HasSuffix(path, "/") {
		builder.WriteByte('/')
	}
	return builder.String(), nil
}

// MicrovmTarcopyPack writes source as a tar stream to out. In
// MicrovmCopyModeItem entries are named from its final path component; in
// MicrovmCopyModeContents they are named relative to the directory, which
// is itself the first entry, `.`.
func MicrovmTarcopyPack(source string, out io.Writer, mode MicrovmCopyMode) error {
	metadata, err := os.Lstat(source)
	if err != nil {
		return fmt.Errorf("stat %s: %w", source, microvmIOError(err))
	}
	builder := tar.NewWriter(out)
	switch mode {
	case MicrovmCopyModeContents:
		// `cp SRC/. DEST` follows a symlink named as SRC to its directory.
		followed, err := os.Stat(source)
		if err != nil {
			return fmt.Errorf("stat %s: %w", source, microvmIOError(err))
		}
		if !followed.IsDir() {
			return fmt.Errorf("%s is not a directory", source)
		}
		if err := microvmTarAppendDirAll(builder, ".", source); err != nil {
			return fmt.Errorf("pack the contents of %s: %w", source, err)
		}
	default:
		name, ok := microvmPathFileName(source)
		if !ok {
			return fmt.Errorf("%s has no final path component to copy", source)
		}
		if metadata.IsDir() {
			if err := microvmTarAppendDirAll(builder, name, source); err != nil {
				return fmt.Errorf("pack directory %s: %w", source, err)
			}
		} else if err := microvmTarAppendPathWithName(builder, source, name); err != nil {
			return fmt.Errorf("pack %s: %w", source, err)
		}
	}
	if err := builder.Close(); err != nil {
		return fmt.Errorf("finish tar stream: %w", microvmIOError(err))
	}
	return nil
}

// microvmTarHeader is the header the tar crate builds from a file's
// metadata in its complete mode.
func microvmTarHeader(name string, stat *unix.Stat_t) (*tar.Header, error) {
	headerName, err := microvmTarHeaderPath(name, false)
	if err != nil {
		return nil, err
	}
	header := &tar.Header{
		Name:    headerName,
		Mode:    int64(stat.Mode),
		Uid:     int(stat.Uid),
		Gid:     int(stat.Gid),
		ModTime: time.Unix(stat.Mtim.Sec, 0),
		Format:  tar.FormatGNU,
	}
	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFREG:
		header.Typeflag = tar.TypeReg
		header.Size = stat.Size
	case unix.S_IFLNK:
		header.Typeflag = tar.TypeSymlink
	case unix.S_IFCHR:
		header.Typeflag = tar.TypeChar
	case unix.S_IFBLK:
		header.Typeflag = tar.TypeBlock
	case unix.S_IFDIR:
		header.Typeflag = tar.TypeDir
	case unix.S_IFIFO:
		header.Typeflag = tar.TypeFifo
	default:
		header.Typeflag = ' '
	}
	return header, nil
}

func microvmLstat(path string) (*unix.Stat_t, error) {
	var stat unix.Stat_t
	if err := unix.Lstat(path, &stat); err != nil {
		return nil, microvmIOError(&os.PathError{Op: "lstat", Path: path, Err: err})
	}
	return &stat, nil
}

func microvmStat(path string) (*unix.Stat_t, error) {
	var stat unix.Stat_t
	if err := unix.Stat(path, &stat); err != nil {
		return nil, microvmIOError(&os.PathError{Op: "stat", Path: path, Err: err})
	}
	return &stat, nil
}

// microvmTarAppendDirAll is the tar crate's append_dir_all without
// following symlinks: each directory's header, then its entries in the
// order the directory lists them, last first.
func microvmTarAppendDirAll(builder *tar.Writer, path string, source string) error {
	type pending struct {
		source    string
		relative  string
		isDir     bool
		isSymlink bool
	}
	stack := []pending{{source: source, isDir: true}}
	for len(stack) > 0 {
		item := stack[len(stack)-1]
		stack = stack[:len(stack)-1]
		// For the directory itself the relative path is empty, and, as with
		// Rust's Path::join, the name then ends with a separator.
		destination := microvmRustJoin(path, item.relative)
		switch {
		case item.isDir:
			directory, err := os.Open(item.source)
			if err != nil {
				return microvmIOError(err)
			}
			entries, err := directory.ReadDir(-1)
			_ = directory.Close()
			if err != nil {
				return microvmIOError(err)
			}
			for _, entry := range entries {
				stack = append(stack, pending{
					source:    microvmRustJoin(item.source, entry.Name()),
					relative:  microvmRustJoin(item.relative, entry.Name()),
					isDir:     entry.Type().IsDir(),
					isSymlink: entry.Type()&os.ModeSymlink != 0,
				})
			}
			stat, err := microvmStat(item.source)
			if err != nil {
				return err
			}
			if err := microvmTarAppendHeader(builder, destination, stat, ""); err != nil {
				return err
			}
		case item.isSymlink:
			stat, err := microvmLstat(item.source)
			if err != nil {
				return err
			}
			target, err := os.Readlink(item.source)
			if err != nil {
				return microvmIOError(err)
			}
			if err := microvmTarAppendHeader(builder, destination, stat, target); err != nil {
				return err
			}
		default:
			stat, err := microvmStat(item.source)
			if err != nil {
				return err
			}
			if stat.Mode&unix.S_IFMT != unix.S_IFREG {
				if err := microvmTarAppendSpecial(builder, destination, stat); err != nil {
					return err
				}
				continue
			}
			if err := microvmTarAppendFile(builder, destination, item.source); err != nil {
				return err
			}
		}
	}
	return nil
}

// microvmTarAppendPathWithName is the tar crate's append_path_with_name
// without following symlinks.
func microvmTarAppendPathWithName(builder *tar.Writer, path string, name string) error {
	stat, err := microvmLstat(path)
	if err != nil {
		return fmt.Errorf("%s when getting metadata for %s", err.Error(), path)
	}
	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFREG:
		return microvmTarAppendFile(builder, name, path)
	case unix.S_IFDIR:
		return microvmTarAppendHeader(builder, name, stat, "")
	case unix.S_IFLNK:
		target, err := os.Readlink(path)
		if err != nil {
			return microvmIOError(err)
		}
		return microvmTarAppendHeader(builder, name, stat, target)
	}
	// The crate names a special file by its source path here.
	return microvmTarAppendSpecial(builder, path, stat)
}

// microvmTarAppendHeader writes a header-only entry: a directory, or a
// symlink to target.
func microvmTarAppendHeader(builder *tar.Writer, name string, stat *unix.Stat_t, target string) error {
	header, err := microvmTarHeader(name, stat)
	if err != nil {
		return err
	}
	header.Size = 0
	if target != "" {
		if header.Linkname, err = microvmTarHeaderPath(target, true); err != nil {
			return err
		}
	}
	if err := builder.WriteHeader(header); err != nil {
		return microvmIOError(err)
	}
	return nil
}

func microvmTarAppendSpecial(builder *tar.Writer, name string, stat *unix.Stat_t) error {
	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFSOCK:
		return fmt.Errorf("%s: socket can not be archived", name)
	case unix.S_IFIFO, unix.S_IFCHR, unix.S_IFBLK:
	default:
		return fmt.Errorf("%s has unknown file type", name)
	}
	header, err := microvmTarHeader(name, stat)
	if err != nil {
		return err
	}
	header.Size = 0
	header.Devmajor = int64(unix.Major(stat.Rdev))
	header.Devminor = int64(unix.Minor(stat.Rdev))
	if err := builder.WriteHeader(header); err != nil {
		return microvmIOError(err)
	}
	return nil
}

func microvmTarAppendFile(builder *tar.Writer, name string, path string) error {
	file, err := os.Open(path)
	if err != nil {
		return microvmIOError(err)
	}
	defer file.Close()
	var stat unix.Stat_t
	if err := unix.Fstat(int(file.Fd()), &stat); err != nil {
		return microvmIOError(err)
	}
	header, err := microvmTarHeader(name, &stat)
	if err != nil {
		return err
	}
	if err := builder.WriteHeader(header); err != nil {
		return microvmIOError(err)
	}
	if _, err := io.CopyN(builder, file, header.Size); err != nil {
		return microvmIOError(err)
	}
	return nil
}

// microvmTarEntryTypeName is the tar crate's EntryType Debug name for a
// type flag.
func microvmTarEntryTypeName(flag byte) string {
	switch flag {
	case tar.TypeReg, tar.TypeRegA:
		return "Regular"
	case tar.TypeLink:
		return "Link"
	case tar.TypeSymlink:
		return "Symlink"
	case tar.TypeChar:
		return "Char"
	case tar.TypeBlock:
		return "Block"
	case tar.TypeDir:
		return "Directory"
	case tar.TypeFifo:
		return "Fifo"
	case tar.TypeCont:
		return "Continuous"
	case tar.TypeGNULongName:
		return "GNULongName"
	case tar.TypeGNULongLink:
		return "GNULongLink"
	case tar.TypeGNUSparse:
		return "GNUSparse"
	case tar.TypeXGlobalHeader:
		return "XGlobalHeader"
	case tar.TypeXHeader:
		return "XHeader"
	}
	return fmt.Sprintf("__Nonexhaustive(%d)", flag)
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
	// Following a symlinked destination directory is on purpose: that is
	// the destination the caller named. Entries below it never follow links.
	var root *os.File
	var rename *string
	rootCreated := false
	if copyMode == MicrovmCopyModeContents {
		var err error
		if root, rootCreated, err = microvmTarcopyContentsDestination(destination); err != nil {
			return err
		}
	} else {
		rootPath, newName, err := microvmTarcopyItemDestination(destination)
		if err != nil {
			return err
		}
		if root, err = microvmTarcopyOpenPath(unix.AT_FDCWD, rootPath, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0); err != nil {
			return fmt.Errorf("open %s: %w", rootPath, microvmIOError(err))
		}
		rename = newName
	}
	defer root.Close()
	archive := tar.NewReader(reader)
	for {
		header, err := archive.Next()
		if err == io.EOF {
			break
		}
		// An insecure name is ours to judge, as Rust's tar crate leaves it.
		if err != nil && !(errors.Is(err, tar.ErrInsecurePath) && header != nil) {
			return fmt.Errorf("read tar entry: %w", microvmIOError(err))
		}
		var relative string
		if copyMode == MicrovmCopyModeContents {
			relative, err = microvmTarcopyContentsRelative(header.Name)
		} else {
			relative, err = microvmTarcopyRenameTop(header.Name, rename)
		}
		if err != nil {
			return err
		}
		mtime := uint64(header.ModTime.Unix())
		if relative == "" {
			// The contents form's `.` entry stands for the destination
			// itself. An existing destination keeps its own metadata, as
			// with Podman.
			if rootCreated {
				if err := microvmTarcopyApplyMetadata(root, uint32(header.Mode)&0o1777, owner, &mtime, destination); err != nil {
					return err
				}
			}
			continue
		}
		if err := microvmTarcopyUnpackEntry(archive, header, root, relative, owner, mtime); err != nil {
			return err
		}
	}
	return nil
}

// microvmTarcopyUnpackEntry creates one entry at relative below root.
func microvmTarcopyUnpackEntry(archive *tar.Reader, header *tar.Header, root *os.File, relative string, owner *MicrovmOwner, mtime uint64) error {
	parentPath := microvmPathParent(relative)
	name, ok := microvmPathFileName(relative)
	if !ok {
		return fmt.Errorf("tar entry %s has no name", relative)
	}
	parent, err := microvmTarcopyOpenParents(root, parentPath)
	if err != nil {
		return err
	}
	defer parent.Close()
	mode := uint32(header.Mode) & 0o1777
	var created *os.File
	switch header.Typeflag {
	case tar.TypeDir:
		kind, exists, err := microvmTarcopyStatAt(parent, name)
		if err != nil {
			return err
		}
		switch {
		case exists && kind == unix.S_IFDIR:
		case exists:
			if err := microvmTarcopyUnlinkAt(parent, name, 0); err != nil {
				return err
			}
			if err := microvmTarcopyMakeDirAt(parent, name); err != nil {
				return err
			}
		default:
			if err := microvmTarcopyMakeDirAt(parent, name); err != nil {
				return err
			}
		}
		created, err = microvmTarcopyOpenPath(int(parent.Fd()), name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
		if err != nil {
			return fmt.Errorf("open %s: %w", relative, microvmIOError(err))
		}
	case tar.TypeReg, tar.TypeCont:
		kind, exists, err := microvmTarcopyStatAt(parent, name)
		if err != nil {
			return err
		}
		if exists && kind == unix.S_IFDIR {
			return fmt.Errorf("refusing to replace directory %s with a file", relative)
		}
		if exists {
			if err := microvmTarcopyUnlinkAt(parent, name, 0); err != nil {
				return err
			}
		}
		created, err = microvmTarcopyOpenPath(int(parent.Fd()), name, unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0o600)
		if err != nil {
			return fmt.Errorf("create %s: %w", relative, microvmIOError(err))
		}
		if _, err := io.Copy(created, archive); err != nil {
			_ = created.Close()
			return fmt.Errorf("write %s: %w", relative, microvmIOError(err))
		}
	case tar.TypeSymlink:
		if header.Linkname == "" {
			return fmt.Errorf("symlink entry %s has no target", relative)
		}
		kind, exists, err := microvmTarcopyStatAt(parent, name)
		if err != nil {
			return err
		}
		if exists && kind == unix.S_IFDIR {
			return fmt.Errorf("refusing to replace directory %s with a symlink", relative)
		}
		if exists {
			if err := microvmTarcopyUnlinkAt(parent, name, 0); err != nil {
				return err
			}
		}
		if strings.ContainsRune(header.Linkname, 0) || strings.ContainsRune(name, 0) {
			return errors.New("a path contains a NUL byte")
		}
		if err := unix.Symlinkat(header.Linkname, int(parent.Fd()), name); err != nil {
			return fmt.Errorf("create symlink %s: %w", relative, microvmIOError(err))
		}
		if owner != nil {
			// AT_SYMLINK_NOFOLLOW keeps the change on the link.
			if err := unix.Fchownat(int(parent.Fd()), name, int(owner.UID), int(owner.GID), unix.AT_SYMLINK_NOFOLLOW); err != nil {
				return fmt.Errorf("change owner of %s: %w", relative, microvmIOError(err))
			}
		}
		return nil
	default:
		return fmt.Errorf("unsupported tar entry type %s for %s", microvmTarEntryTypeName(header.Typeflag), relative)
	}
	defer created.Close()
	return microvmTarcopyApplyMetadata(created, mode, owner, &mtime, relative)
}

// microvmTarcopyApplyMetadata gives an entry exactly the mode, owner and
// modification time (seconds, nil to leave it) it carried.
func microvmTarcopyApplyMetadata(fd *os.File, mode uint32, owner *MicrovmOwner, mtime *uint64, label string) error {
	raw := int(fd.Fd())
	// The creation mode is filtered by the umask; set it exactly.
	if err := unix.Fchmod(raw, mode); err != nil {
		return fmt.Errorf("set mode of %s: %w", label, microvmIOError(err))
	}
	if owner != nil {
		if err := unix.Fchown(raw, int(owner.UID), int(owner.GID)); err != nil {
			return fmt.Errorf("change owner of %s: %w", label, microvmIOError(err))
		}
	}
	if mtime != nil {
		times := [2]unix.Timespec{
			{Sec: 0, Nsec: unix.UTIME_OMIT},
			{Sec: int64(*mtime), Nsec: 0},
		}
		// futimens(fd, times) is utimensat(fd, NULL, times, 0).
		_, _, errno := unix.Syscall6(unix.SYS_UTIMENSAT, uintptr(raw), 0, uintptr(unsafe.Pointer(&times[0])), 0, 0, 0)
		if errno != 0 {
			return fmt.Errorf("set modification time of %s: %w", label, microvmIOError(errno))
		}
	}
	return nil
}

// microvmTarcopyItemDestination is where an item copy lands: inside an
// existing directory (rename nil), or as a new name in an existing parent.
func microvmTarcopyItemDestination(destination string) (root string, rename *string, err error) {
	if info, statErr := os.Stat(destination); statErr == nil && info.IsDir() {
		return destination, nil, nil
	}
	name, ok := microvmPathFileName(destination)
	if !ok {
		return "", nil, fmt.Errorf("%s has no final path component", destination)
	}
	parent := microvmPathParent(destination)
	if parent == "" {
		parent = "."
	}
	if info, statErr := os.Stat(parent); statErr != nil || !info.IsDir() {
		return "", nil, fmt.Errorf("destination directory %s does not exist", parent)
	}
	return parent, &name, nil
}

// microvmTarcopyContentsDestination opens the directory a contents copy
// fills and reports whether this call created it. A missing destination is
// created in its existing parent; an existing non-directory is refused.
func microvmTarcopyContentsDestination(destination string) (*os.File, bool, error) {
	const directory = unix.O_RDONLY | unix.O_DIRECTORY | unix.O_CLOEXEC
	info, err := os.Stat(destination)
	switch {
	case err == nil && info.IsDir():
		fd, err := microvmTarcopyOpenPath(unix.AT_FDCWD, destination, directory, 0)
		if err != nil {
			return nil, false, fmt.Errorf("open %s: %w", destination, microvmIOError(err))
		}
		return fd, false, nil
	case err == nil:
		return nil, false, fmt.Errorf("destination %s exists and is not a directory", destination)
	case os.IsNotExist(err):
		name, ok := microvmPathFileName(destination)
		if !ok {
			return nil, false, fmt.Errorf("%s has no final path component", destination)
		}
		parentPath := microvmPathParent(destination)
		if parentPath == "" {
			parentPath = "."
		}
		if parentInfo, statErr := os.Stat(parentPath); statErr != nil || !parentInfo.IsDir() {
			return nil, false, fmt.Errorf("destination directory %s does not exist", parentPath)
		}
		parent, err := microvmTarcopyOpenPath(unix.AT_FDCWD, parentPath, directory, 0)
		if err != nil {
			return nil, false, fmt.Errorf("open %s: %w", parentPath, microvmIOError(err))
		}
		defer parent.Close()
		if err := microvmTarcopyMakeDirAt(parent, name); err != nil {
			return nil, false, err
		}
		fd, err := microvmTarcopyOpenPath(int(parent.Fd()), name, directory|unix.O_NOFOLLOW, 0)
		if err != nil {
			return nil, false, fmt.Errorf("open %s: %w", destination, microvmIOError(err))
		}
		return fd, true, nil
	}
	return nil, false, fmt.Errorf("stat destination %s: %w", destination, microvmIOError(err))
}

// microvmTarcopyContentsRelative is an entry path of a contents copy,
// without its leading `./`. Empty for the directory itself.
func microvmTarcopyContentsRelative(path string) (string, error) {
	var result []microvmComponent
	for _, component := range microvmPathComponents(path) {
		switch component.kind {
		case microvmComponentNormal:
			result = append(result, component)
		case microvmComponentCurDir:
		default:
			return "", fmt.Errorf("tar entry path %s leaves its destination", path)
		}
	}
	return microvmJoinComponents(result), nil
}

// microvmTarcopyRenameTop is an entry path of an item copy with its first
// component replaced by rename, when given.
func microvmTarcopyRenameTop(path string, rename *string) (string, error) {
	components := microvmPathComponents(path)
	if len(components) == 0 || components[0].kind != microvmComponentNormal {
		return "", fmt.Errorf("tar entry path %s is not a relative path", path)
	}
	result := []microvmComponent{components[0]}
	if rename != nil {
		result[0].name = *rename
	}
	for _, component := range components[1:] {
		switch component.kind {
		case microvmComponentNormal:
			result = append(result, component)
		case microvmComponentCurDir:
		default:
			return "", fmt.Errorf("tar entry path %s leaves its destination", path)
		}
	}
	return microvmJoinComponents(result), nil
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
	var stat unix.Stat_t
	err = unix.Fstatat(int(dir.Fd()), name, &stat, unix.AT_SYMLINK_NOFOLLOW)
	if err == nil {
		return stat.Mode & unix.S_IFMT, true, nil
	}
	if errors.Is(err, syscall.ENOENT) {
		return 0, false, nil
	}
	return 0, false, fmt.Errorf("stat a tar destination: %w", microvmIOError(err))
}

func microvmTarcopyUnlinkAt(dir *os.File, name string, flags int) error {
	if err := unix.Unlinkat(int(dir.Fd()), name, flags); err != nil {
		return fmt.Errorf("replace a tar destination: %w", microvmIOError(err))
	}
	return nil
}

func microvmTarcopyMakeDirAt(dir *os.File, name string) error {
	if err := unix.Mkdirat(int(dir.Fd()), name, 0o700); err != nil {
		return fmt.Errorf("create a tar directory: %w", microvmIOError(err))
	}
	return nil
}

// microvmTarcopyOpenParents opens (creating missing ones) the directories
// of relative below root, one component at a time, each relative to the
// previous descriptor and refusing symlinks.
func microvmTarcopyOpenParents(root *os.File, relative string) (*os.File, error) {
	duplicate, err := unix.FcntlInt(root.Fd(), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		return nil, fmt.Errorf("duplicate the destination descriptor: %w", microvmIOError(err))
	}
	current := os.NewFile(uintptr(duplicate), relative)
	for _, component := range microvmPathComponents(relative) {
		if component.kind != microvmComponentNormal {
			continue
		}
		const flags = unix.O_RDONLY | unix.O_DIRECTORY | unix.O_NOFOLLOW | unix.O_CLOEXEC
		next, err := microvmTarcopyOpenPath(int(current.Fd()), component.name, flags, 0)
		switch {
		case err == nil:
		case microvmTarcopyIsErrno(err, unix.ENOENT):
			if err := microvmTarcopyMakeDirAt(current, component.name); err != nil {
				_ = current.Close()
				return nil, err
			}
			if next, err = microvmTarcopyOpenPath(int(current.Fd()), component.name, flags, 0); err != nil {
				_ = current.Close()
				return nil, microvmIOError(err)
			}
		case microvmTarcopyIsErrno(err, unix.ELOOP) || microvmTarcopyIsErrno(err, unix.ENOTDIR):
			_ = current.Close()
			return nil, fmt.Errorf("refusing to unpack through %s: it is not a directory", relative)
		default:
			_ = current.Close()
			return nil, microvmIOError(err)
		}
		_ = current.Close()
		current = next
	}
	return current, nil
}

func microvmTarcopyIsErrno(err error, errno unix.Errno) bool {
	return errors.Is(err, errno)
}
