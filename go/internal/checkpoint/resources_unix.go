// Port of mj-checkpoint/src/resources.rs: the `cfg(unix)` stream producer.

//go:build unix

package checkpoint

import (
	"archive/tar"
	"io"
)

// StreamResource produces a tar.gz stream of the directory source without
// materializing it on disk or in memory, and hands it to consume. A
// goroutine writes the archive into one end of a socket pair while consume
// reads the other; consume's error wins over the producer's.
func StreamResource(source string, consume func(io.Reader) error) error {
	panic("unported: mj-checkpoint/src/resources.rs stream_resource")
}

// writeResourceStream writes source as a gzip-compressed (fastest level)
// tar archive to output.
func writeResourceStream(source string, output io.Writer) error {
	panic("unported: mj-checkpoint/src/resources.rs write_resource_stream")
}

// appendDirectory appends directory's entries to archive in name order: an
// empty directory below root as a 0755 directory entry, regular files with
// their metadata. Symbolic links and other file types are refused.
func appendDirectory(archive *tar.Writer, root, directory string, entries *int) error {
	panic("unported: mj-checkpoint/src/resources.rs append_directory")
}

// safeRelativePath is path relative to root, refusing an empty path or any
// component other than a plain name.
func safeRelativePath(root, path string) (string, error) {
	panic("unported: mj-checkpoint/src/resources.rs safe_relative_path")
}
