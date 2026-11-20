// Port of mj-checkpoint/src/resources.rs: the `cfg(unix)` stream producer.

//go:build unix

package checkpoint

import (
	"archive/tar"
	"compress/gzip"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

// StreamResource produces a tar.gz stream of the directory source without
// materializing it on disk or in memory, and hands it to consume. A
// goroutine writes the archive into one end of a pipe while consume reads
// the other; consume's error wins over the producer's. Once consume returns,
// the read end is closed, so a producer it stopped reading from fails
// instead of blocking.
func StreamResource(source string, consume func(io.Reader) error) error {
	if info, err := os.Stat(source); err != nil || !info.IsDir() {
		return fmt.Errorf("resource source is not a directory: %s", source)
	}
	reader, writer := io.Pipe()
	produced := make(chan error, 1)
	go func() {
		var err error
		defer func() {
			if recover() != nil {
				err = errors.New("resource compression thread panicked")
			}
			// The reader sees the end of the stream however the producer
			// ended, as when Rust drops the writing socket.
			writer.Close()
			produced <- err
		}()
		err = writeResourceStream(source, writer)
	}()
	consumed := consume(reader)
	reader.Close()
	producerErr := <-produced
	if consumed != nil {
		return consumed
	}
	return producerErr
}

// writeResourceStream writes source as a gzip-compressed (fastest level)
// tar archive to output.
func writeResourceStream(source string, output io.Writer) error {
	encoder, err := gzip.NewWriterLevel(output, gzip.BestSpeed)
	if err != nil {
		return err
	}
	archive := tar.NewWriter(encoder)
	entries := 0
	if err := appendDirectory(archive, source, source, &entries); err != nil {
		return err
	}
	if err := archive.Close(); err != nil {
		return fmt.Errorf("finish resource tar stream: %w", err)
	}
	if err := encoder.Close(); err != nil {
		return fmt.Errorf("finish resource gzip stream: %w", err)
	}
	return nil
}

// appendDirectory appends directory's entries to archive in name order: an
// empty directory below root as a 0755 directory entry, regular files with
// their metadata. Symbolic links and other file types are refused.
func appendDirectory(archive *tar.Writer, root, directory string, entries *int) error {
	// os.ReadDir sorts by file name, as Rust sorts the entries.
	children, err := os.ReadDir(directory)
	if err != nil {
		return fmt.Errorf("read resource directory %s: %w", directory, err)
	}
	if len(children) == 0 && directory != root {
		if err := ensureEntryLimit(*entries); err != nil {
			return err
		}
		relative, err := safeRelativePath(root, directory)
		if err != nil {
			return err
		}
		header := &tar.Header{
			Typeflag: tar.TypeDir,
			Name:     relative,
			Mode:     0o755,
			Format:   tar.FormatGNU,
		}
		if err := archive.WriteHeader(header); err != nil {
			return err
		}
		*entries++
	}
	for _, child := range children {
		if err := ensureEntryLimit(*entries); err != nil {
			return err
		}
		path := filepath.Join(directory, child.Name())
		info, err := os.Lstat(path)
		if err != nil {
			return err
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return fmt.Errorf("resource contains unsupported symbolic link: %s", path)
		}
		if info.IsDir() {
			if err := appendDirectory(archive, root, path, entries); err != nil {
				return err
			}
			continue
		}
		if !info.Mode().IsRegular() {
			return fmt.Errorf("resource entry is not a regular file: %s", path)
		}
		relative, err := safeRelativePath(root, path)
		if err != nil {
			return err
		}
		// The metadata tar's Header::set_metadata records: the raw mode,
		// the owner and the modification second.
		header := &tar.Header{
			Typeflag: tar.TypeReg,
			Name:     relative,
			Size:     info.Size(),
			ModTime:  time.Unix(info.ModTime().Unix(), 0),
			Format:   tar.FormatGNU,
		}
		if stat, ok := info.Sys().(*syscall.Stat_t); ok {
			header.Mode = int64(stat.Mode)
			header.Uid = int(stat.Uid)
			header.Gid = int(stat.Gid)
		} else {
			header.Mode = int64(info.Mode().Perm())
		}
		if err := archive.WriteHeader(header); err != nil {
			return err
		}
		if err := copyResourceFile(archive, path); err != nil {
			return err
		}
		*entries++
	}
	return nil
}

// copyResourceFile writes the contents of the file at path to archive.
func copyResourceFile(archive io.Writer, path string) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	_, err = io.Copy(archive, file)
	return err
}

// safeRelativePath is path relative to root, refusing an empty path or any
// component other than a plain name.
func safeRelativePath(root, path string) (string, error) {
	relative, err := filepath.Rel(root, path)
	if err != nil || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		// Rust's Path::strip_prefix error.
		return "", errors.New("prefix not found")
	}
	if relative == "." {
		return "", errors.New("resource archive entry is empty")
	}
	relative = filepath.ToSlash(relative)
	if !isSafeArchivePath(relative) {
		return "", fmt.Errorf("unsafe resource archive path: %s", relative)
	}
	return relative, nil
}
