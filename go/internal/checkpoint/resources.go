// Port of mj-checkpoint/src/resources.rs: the parts for every platform.
//
// Streaming snapshots of directories attached to remote session targets.
// StreamResource is in resources_unix.go and resources_other.go.

package checkpoint

import (
	"archive/tar"
	"compress/gzip"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
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
	if err := validateDestination(destination); err != nil {
		return err
	}
	if err := os.MkdirAll(destination, 0o777); err != nil {
		return fmt.Errorf("create resource destination %s: %w", destination, err)
	}
	decoder, err := gzip.NewReader(input)
	if err != nil {
		return fmt.Errorf("read resource tar entry: %w", err)
	}
	// One gzip member, as flate2's GzDecoder reads.
	decoder.Multistream(false)
	archive := tar.NewReader(decoder)
	var expanded uint64
	for entryIndex := 0; ; entryIndex++ {
		header, err := archive.Next()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err := ensureEntryLimit(entryIndex); err != nil {
			return err
		}
		if err != nil {
			return fmt.Errorf("read resource tar entry: %w", err)
		}
		isFile := header.Typeflag == tar.TypeReg
		isDir := header.Typeflag == tar.TypeDir
		if !isFile && !isDir {
			return errors.New("resource stream contains unsupported entry type")
		}
		relative := header.Name
		if !isSafeArchivePath(relative) {
			return fmt.Errorf("unsafe resource archive path: %s", relative)
		}
		size := uint64(max(header.Size, 0))
		if expanded > math.MaxUint64-size {
			return errors.New("resource expanded size overflow")
		}
		expanded += size
		if expanded > maxExpandedBytes {
			return errors.New("resource expanded size exceeds 50 GiB")
		}
		output := filepath.Join(destination, filepath.FromSlash(relative))
		if err := ensureSafeParent(destination, filepath.Dir(output)); err != nil {
			return err
		}
		if err := ensureNotSymlink(output); err != nil {
			return err
		}
		if isDir {
			if err := os.MkdirAll(output, 0o777); err != nil {
				return err
			}
			continue
		}
		if err := os.MkdirAll(filepath.Dir(output), 0o777); err != nil {
			return err
		}
		if err := writeResourceFile(output, archive); err != nil {
			return err
		}
		if runtime.GOOS != "windows" {
			if err := os.Chmod(output, fs.FileMode(header.Mode&0o777)); err != nil {
				return err
			}
		}
	}
}

// writeResourceFile creates or truncates path and copies the entry's bytes
// into it.
func writeResourceFile(path string, entry io.Reader) error {
	file, err := os.Create(path)
	if err != nil {
		return err
	}
	if _, err := io.Copy(file, entry); err != nil {
		file.Close()
		return err
	}
	return file.Close()
}

// isSafeArchivePath reports whether an archive entry's path is non-empty and
// made only of plain names, as Rust's Path::components sees it: a leading
// `/`, a leading `.` or any `..` is refused, while empty segments and `.`
// after the first segment are not components at all.
func isSafeArchivePath(path string) bool {
	if path == "" {
		return false
	}
	segments := strings.Split(path, "/")
	if segments[0] == "" || segments[0] == "." {
		return false
	}
	for _, segment := range segments {
		if segment == ".." {
			return false
		}
	}
	return true
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
	if !filepath.IsAbs(destination) {
		return errors.New("resource destination must be absolute")
	}
	names := pathNames(destination)
	if len(names) == 0 && filepath.VolumeName(destination) == "" {
		return errors.New("resource destination cannot be the filesystem root")
	}
	if slices.Contains(names, "..") {
		return errors.New("resource destination cannot contain '..'")
	}
	return nil
}

// pathNames is the components of path after its volume and root, as Rust's
// Path::components yields them: empty segments and `.` are not components.
func pathNames(path string) []string {
	path = path[len(filepath.VolumeName(path)):]
	names := []string{}
	separator := func(r rune) bool { return r < 0x80 && os.IsPathSeparator(uint8(r)) }
	for _, name := range strings.FieldsFunc(path, separator) {
		if name != "." {
			names = append(names, name)
		}
	}
	return names
}

// ensureSafeParent refuses a parent below root reached through a symbolic
// link.
func ensureSafeParent(root, parent string) error {
	relative, err := filepath.Rel(root, parent)
	if err != nil || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		// Rust's Path::strip_prefix error.
		return errors.New("prefix not found")
	}
	current := root
	for _, name := range pathNames(relative) {
		current = filepath.Join(current, name)
		if info, err := os.Lstat(current); err == nil && info.Mode()&os.ModeSymlink != 0 {
			return fmt.Errorf("resource destination traverses symbolic link: %s", current)
		}
	}
	return nil
}
