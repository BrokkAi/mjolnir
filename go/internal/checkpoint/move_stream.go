// Port of mj-checkpoint/src/move_stream.rs.
//
// Single-stream framing for Move archive + `.mj/keep/` transfer.
//
// Wire format:
//  1. 8-byte magic header: `MJMV0001`
//  2. `u64` little-endian `archive_len`, followed by `archive_len` raw bytes
//     of the checkpoint archive.
//  3. `u64` little-endian `keep_tar_len`, followed by `keep_tar_len` raw
//     bytes of the `.mj/keep/` tar stream (0 bytes when no keep directory is
//     sent).
//  4. EOF (trailing bytes are rejected).
//
// Both parts travel in one stream under one SSH grant and route. The
// receiver verifies the archive's SHA-256 and extracts `.mj/keep/` with
// strict symlink/path-containment checks, persisting the archive atomically
// only once both parts arrive complete and verified.

package checkpoint

import (
	"archive/tar"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"hash"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MoveStreamMagic is the magic header identifying version 1 of the Mjolnir
// Move stream framing.
var MoveStreamMagic = [8]byte{'M', 'J', 'M', 'V', '0', '0', '0', '1'}

const moveStreamBufSize = 64 * 1024

// MoveStreamExportReport is the summary returned by the sender after
// writing a framed Move stream.
type MoveStreamExportReport struct {
	ArchiveBytes  uint64 `json:"archive_bytes"`
	ArchiveSHA256 string `json:"archive_sha256"`
	KeepTarBytes  uint64 `json:"keep_tar_bytes"`
}

func (r *MoveStreamExportReport) UnmarshalJSON(data []byte) error {
	type plain MoveStreamExportReport
	return checkpointStrictDecode(data, (*plain)(r), false)
}

// MoveStreamReceipt is the verified receipt returned by the receiver after
// consuming a framed Move stream.
type MoveStreamReceipt struct {
	ArchiveBytes  uint64 `json:"archive_bytes"`
	ArchiveSHA256 string `json:"archive_sha256"`
	KeepTarBytes  uint64 `json:"keep_tar_bytes"`
}

func (r *MoveStreamReceipt) UnmarshalJSON(data []byte) error {
	type plain MoveStreamReceipt
	return checkpointStrictDecode(data, (*plain)(r), false)
}

// WriteMoveStream writes a framed Move stream (archive + optional
// `.mj/keep/` tar) to writer. skip paths are relative to keepDir and
// excluded from the tar stream.
func WriteMoveStream(writer io.Writer, archivePath string, keepDir *string, skip []string) (MoveStreamExportReport, error) {
	archiveFile, err := os.Open(archivePath)
	if err != nil {
		return MoveStreamExportReport{}, fmt.Errorf("open archive %s: %w", archivePath, err)
	}
	defer archiveFile.Close()
	info, err := archiveFile.Stat()
	if err != nil {
		return MoveStreamExportReport{}, fmt.Errorf("stat archive %s: %w", archivePath, err)
	}
	archiveLen := uint64(info.Size())

	directory := ""
	if keepDir != nil {
		directory = *keepDir
	}
	keep, err := core.NewMoveKeep(directory, skip)
	if err != nil {
		return MoveStreamExportReport{}, err
	}

	// Build the keep tar into a temporary file on disk so we know its exact
	// length prefix without buffering large scratch trees in memory.
	var keepTarTemp *os.File
	defer func() {
		if keepTarTemp != nil {
			keepTarTemp.Close()
			os.Remove(keepTarTemp.Name())
		}
	}()
	var keepTarLen uint64
	if keepDir != nil {
		if err := ensureNoSymlinkedKeepAncestors(*keepDir); err != nil {
			return MoveStreamExportReport{}, err
		}
		if checkpointExists(*keepDir) {
			temp, err := os.CreateTemp("", ".tmp")
			if err != nil {
				return MoveStreamExportReport{}, fmt.Errorf("create temp keep tar: %w", err)
			}
			keepTarTemp = temp
			written, err := buildKeepTar(keep, temp)
			if err != nil {
				return MoveStreamExportReport{}, err
			}
			if err := temp.Sync(); err != nil {
				return MoveStreamExportReport{}, fmt.Errorf("sync temp keep tar: %w", err)
			}
			keepTarLen = written
		}
	}

	if _, err := writer.Write(MoveStreamMagic[:]); err != nil {
		return MoveStreamExportReport{}, fmt.Errorf("write move stream magic: %w", err)
	}
	if _, err := writer.Write(binary.LittleEndian.AppendUint64(nil, archiveLen)); err != nil {
		return MoveStreamExportReport{}, fmt.Errorf("write archive length: %w", err)
	}

	hasher := sha256.New()
	copiedArchive, err := copyWithHash(archiveFile, writer, hasher)
	if err != nil {
		return MoveStreamExportReport{}, fmt.Errorf("stream archive payload: %w", err)
	}
	if copiedArchive != archiveLen {
		return MoveStreamExportReport{}, fmt.Errorf("archive size changed during export: expected %d, wrote %d", archiveLen, copiedArchive)
	}
	archiveSHA256 := core.LowerHex(hasher.Sum(nil))

	if _, err := writer.Write(binary.LittleEndian.AppendUint64(nil, keepTarLen)); err != nil {
		return MoveStreamExportReport{}, fmt.Errorf("write keep tar length: %w", err)
	}
	if keepTarTemp != nil {
		if _, err := keepTarTemp.Seek(0, io.SeekStart); err != nil {
			return MoveStreamExportReport{}, fmt.Errorf("rewind temp keep tar: %w", err)
		}
		copiedKeep, err := copyWithHash(keepTarTemp, writer, nil)
		if err != nil {
			return MoveStreamExportReport{}, fmt.Errorf("stream keep tar payload: %w", err)
		}
		if copiedKeep != keepTarLen {
			return MoveStreamExportReport{}, fmt.Errorf("keep tar size mismatch during export: expected %d, wrote %d", keepTarLen, copiedKeep)
		}
	}

	if flusher, ok := writer.(interface{ Flush() error }); ok {
		if err := flusher.Flush(); err != nil {
			return MoveStreamExportReport{}, fmt.Errorf("flush move stream: %w", err)
		}
	}
	return MoveStreamExportReport{
		ArchiveBytes:  archiveLen,
		ArchiveSHA256: archiveSHA256,
		KeepTarBytes:  keepTarLen,
	}, nil
}

// ReceiveMoveStream receives and verifies a framed Move stream from reader.
//
// The archive is written to a temporary file beside archiveDest, verified
// against expectedSHA256, and atomically persisted into archiveDest only
// after both the archive and the `.mj/keep/` tar complete cleanly.
func ReceiveMoveStream(reader io.Reader, archiveDest string, expectedSHA256 string, keepDest *string) (MoveStreamReceipt, error) {
	var magic [8]byte
	if _, err := io.ReadFull(reader, magic[:]); err != nil {
		return MoveStreamReceipt{}, fmt.Errorf("read move stream magic: %w", err)
	}
	if magic != MoveStreamMagic {
		return MoveStreamReceipt{}, errors.New("invalid move stream magic header")
	}

	archiveLen, err := readU64LE(reader)
	if err != nil {
		return MoveStreamReceipt{}, fmt.Errorf("read archive length: %w", err)
	}
	archiveParent, ok := checkpointParent(archiveDest)
	if !ok || archiveParent == "" {
		return MoveStreamReceipt{}, errors.New("archive_dest must have a parent directory")
	}
	if err := os.MkdirAll(archiveParent, 0o777); err != nil {
		return MoveStreamReceipt{}, fmt.Errorf("create archive dir %s: %w", archiveParent, err)
	}

	archiveTemp, err := os.CreateTemp(archiveParent, ".tmp")
	if err != nil {
		return MoveStreamReceipt{}, fmt.Errorf("create temp archive in %s: %w", archiveParent, err)
	}
	archivePersisted := false
	defer func() {
		archiveTemp.Close()
		if !archivePersisted {
			os.Remove(archiveTemp.Name())
		}
	}()
	hasher := sha256.New()
	archiveBytes, err := copyExactWithHash(reader, archiveTemp, archiveLen, hasher)
	if err != nil {
		return MoveStreamReceipt{}, fmt.Errorf("receive archive payload: %w", err)
	}
	if err := archiveTemp.Sync(); err != nil {
		return MoveStreamReceipt{}, fmt.Errorf("sync received archive: %w", err)
	}

	archiveSHA256 := core.LowerHex(hasher.Sum(nil))
	if !strings.EqualFold(archiveSHA256, expectedSHA256) || !moveStreamASCII(expectedSHA256) {
		return MoveStreamReceipt{}, fmt.Errorf("archive SHA-256 mismatch: expected %s, got %s", expectedSHA256, archiveSHA256)
	}

	keepTarLen, err := readU64LE(reader)
	if err != nil {
		return MoveStreamReceipt{}, fmt.Errorf("read keep tar length: %w", err)
	}
	stageDir := ""
	defer func() {
		if stageDir != "" {
			os.RemoveAll(stageDir)
		}
	}()
	var dest string
	if keepTarLen > 0 {
		if keepDest == nil {
			return MoveStreamReceipt{}, errors.New("received non-empty keep tar without a keep_dest path")
		}
		dest = *keepDest
		if err := ensureNoSymlinkedKeepAncestors(dest); err != nil {
			return MoveStreamReceipt{}, err
		}
		keepParent, ok := checkpointParent(dest)
		if !ok || keepParent == "" {
			return MoveStreamReceipt{}, errors.New("keep_dest must have a parent directory")
		}
		if err := os.MkdirAll(keepParent, 0o777); err != nil {
			return MoveStreamReceipt{}, fmt.Errorf("create keep parent dir %s: %w", keepParent, err)
		}
		if err := ensureNoSymlinkedKeepAncestors(dest); err != nil {
			return MoveStreamReceipt{}, err
		}
		keepTemp, err := os.CreateTemp("", ".tmp")
		if err != nil {
			return MoveStreamReceipt{}, fmt.Errorf("create temp keep tar receiver: %w", err)
		}
		defer func() {
			keepTemp.Close()
			os.Remove(keepTemp.Name())
		}()
		keepBytes, err := copyExactWithHash(reader, keepTemp, keepTarLen, nil)
		if err != nil {
			return MoveStreamReceipt{}, fmt.Errorf("receive keep tar payload: %w", err)
		}
		if keepBytes != keepTarLen {
			return MoveStreamReceipt{}, errors.New("truncated keep tar payload")
		}
		if err := keepTemp.Sync(); err != nil {
			return MoveStreamReceipt{}, fmt.Errorf("sync temp keep tar receiver: %w", err)
		}
		staged, err := os.MkdirTemp(keepParent, ".mj-keep-stage-")
		if err != nil {
			return MoveStreamReceipt{}, fmt.Errorf("create keep staging dir in %s: %w", keepParent, err)
		}
		stageDir = staged
		if err := ExtractKeepTarStrict(keepTemp.Name(), stageDir); err != nil {
			return MoveStreamReceipt{}, err
		}
	}

	// Ensure there are no unexpected trailing bytes before committing either
	// the archive or the staged keep directory.
	var trailing [1]byte
	extra, err := io.ReadAtLeast(reader, trailing[:], 1)
	if err != nil && !errors.Is(err, io.EOF) {
		return MoveStreamReceipt{}, fmt.Errorf("check move stream EOF: %w", err)
	}
	if extra != 0 {
		return MoveStreamReceipt{}, errors.New("unexpected trailing bytes after move stream")
	}

	// Swap the staged keep directory into place with a rollback guard so
	// that if persisting archiveDest fails, the prior keepDest is restored
	// untouched.
	var keepCommit *stagedKeepCommit
	if stageDir != "" {
		staged := stageDir
		stageDir = ""
		commit, err := swapStagedKeepDir(staged, dest)
		if err != nil {
			return MoveStreamReceipt{}, err
		}
		keepCommit = commit
	}

	archiveTemp.Close()
	if err := os.Rename(archiveTemp.Name(), archiveDest); err != nil {
		if keepCommit != nil {
			keepCommit.rollback()
		}
		return MoveStreamReceipt{}, fmt.Errorf("persist verified archive to %s: %w", archiveDest, err)
	}
	archivePersisted = true

	if keepCommit != nil {
		keepCommit.commit()
	}

	return MoveStreamReceipt{
		ArchiveBytes:  archiveBytes,
		ArchiveSHA256: archiveSHA256,
		KeepTarBytes:  keepTarLen,
	}, nil
}

// moveStreamASCII is whether text is ASCII, as eq_ignore_ascii_case needs
// for two strings of different case to match.
func moveStreamASCII(text string) bool {
	for index := 0; index < len(text); index++ {
		if text[index] >= 0x80 {
			return false
		}
	}
	return true
}

// stagedKeepCommit holds the backup of a replaced keep_dest until
// archive_dest persists. rollback (Rust's Drop without commit) restores
// keep_dest.
type stagedKeepCommit struct {
	dest      string
	backup    string
	hasBackup bool
	committed bool
}

func (c *stagedKeepCommit) commit() {
	c.committed = true
	if c.hasBackup {
		c.hasBackup = false
		_ = os.RemoveAll(c.backup)
	}
}

func (c *stagedKeepCommit) rollback() {
	if c.committed {
		return
	}
	_ = os.RemoveAll(c.dest)
	if c.hasBackup {
		c.hasBackup = false
		_ = os.Rename(c.backup, c.dest)
	}
}

// swapStagedKeepDir swaps stagePath into dest, keeping any prior dest in a
// sibling backup until the returned stagedKeepCommit is committed.
func swapStagedKeepDir(stagePath string, dest string) (*stagedKeepCommit, error) {
	backup := ""
	hasBackup := false
	metadata, err := os.Lstat(dest)
	switch {
	case err == nil:
		// As in Rust, these refusals leave the kept staging directory.
		if checkpointIsSymlink(metadata) {
			return nil, fmt.Errorf("refusing symlink at keep dest %s", dest)
		}
		parent, ok := checkpointParent(dest)
		if !ok || parent == "" {
			return nil, errors.New("keep_dest must have a parent directory")
		}
		backupDir, err := os.MkdirTemp(parent, ".mj-keep-old-")
		if err != nil {
			return nil, fmt.Errorf("create backup dir for keep swap: %w", err)
		}
		_ = os.Remove(backupDir)
		if err := os.Rename(dest, backupDir); err != nil {
			return nil, fmt.Errorf("move existing keep dir %s aside: %w", dest, err)
		}
		backup, hasBackup = backupDir, true
	case errors.Is(err, fs.ErrNotExist):
	default:
		_ = os.RemoveAll(stagePath)
		return nil, fmt.Errorf("lstat keep dest %s: %w", dest, err)
	}

	if err := os.Rename(stagePath, dest); err != nil {
		if hasBackup {
			_ = os.Rename(backup, dest)
		}
		_ = os.RemoveAll(stagePath)
		return nil, fmt.Errorf("commit staged keep dir to %s: %w", dest, err)
	}

	return &stagedKeepCommit{dest: dest, backup: backup, hasBackup: hasBackup}, nil
}

func readU64LE(reader io.Reader) (uint64, error) {
	var buffer [8]byte
	if _, err := io.ReadFull(reader, buffer[:]); err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint64(buffer[:]), nil
}

func copyWithHash(reader io.Reader, writer io.Writer, hasher hash.Hash) (uint64, error) {
	buffer := make([]byte, moveStreamBufSize)
	var total uint64
	for {
		n, err := reader.Read(buffer)
		if n > 0 {
			if hasher != nil {
				hasher.Write(buffer[:n])
			}
			if _, err := writer.Write(buffer[:n]); err != nil {
				return total, err
			}
			total += uint64(n)
		}
		if errors.Is(err, io.EOF) {
			return total, nil
		}
		if err != nil {
			return total, err
		}
	}
}

func copyExactWithHash(reader io.Reader, writer io.Writer, expected uint64, hasher hash.Hash) (uint64, error) {
	buffer := make([]byte, moveStreamBufSize)
	remaining := expected
	for remaining > 0 {
		toRead := min(remaining, uint64(len(buffer)))
		n, err := reader.Read(buffer[:toRead])
		if n > 0 {
			if hasher != nil {
				hasher.Write(buffer[:n])
			}
			if _, err := writer.Write(buffer[:n]); err != nil {
				return 0, err
			}
			remaining -= uint64(n)
			continue
		}
		if err == nil {
			continue
		}
		if errors.Is(err, io.EOF) {
			return 0, fmt.Errorf("truncated move stream: expected %d bytes, got %d", expected, expected-remaining)
		}
		return 0, err
	}
	return expected, nil
}

// ensureNoSymlinkedKeepAncestors verifies that neither keepDir nor its
// checkout-owned ancestors (its immediate parent, and `.mj` when under
// `.../.mj/keep/<session-id>`) are symlinks (including dangling symlinks).
//
// Ancestors above the checkout root (such as `/var` -> `/private/var` on
// macOS) are intentionally left to the OS while any symlink at `.mj`,
// `.mj/keep`, or `.mj/keep/<session-id>` is refused before reading or
// creating directories.
func ensureNoSymlinkedKeepAncestors(keepDir string) error {
	var toCheck []string
	if parent, ok := checkpointParent(keepDir); ok && parent != "" {
		if name, ok := checkpointFileName(parent); ok && name == "keep" {
			if grandparent, ok := checkpointParent(parent); ok && grandparent != "" {
				if name, ok := checkpointFileName(grandparent); ok && name == ".mj" {
					toCheck = append(toCheck, grandparent)
				}
			}
		}
		toCheck = append(toCheck, parent)
	}
	toCheck = append(toCheck, keepDir)

	for _, path := range toCheck {
		metadata, err := os.Lstat(path)
		switch {
		case err == nil:
			if checkpointIsSymlink(metadata) {
				return fmt.Errorf("refusing symlink in keep path or ancestor: %s", path)
			}
			if !metadata.IsDir() {
				return fmt.Errorf("keep path or ancestor is not a directory: %s", path)
			}
		case errors.Is(err, fs.ErrNotExist):
		default:
			return fmt.Errorf("lstat keep path %s: %w", path, err)
		}
	}
	return nil
}

// countingWriter counts the bytes written through it.
type countingWriter struct {
	inner io.Writer
	bytes uint64
}

func (w *countingWriter) Write(data []byte) (int, error) {
	n, err := w.inner.Write(data)
	w.bytes += uint64(n)
	return n, err
}

func buildKeepTar(keep core.MoveKeep, writer io.Writer) (uint64, error) {
	counting := &countingWriter{inner: writer}
	builder := tar.NewWriter(counting)
	if err := appendKeepDirRecursive(builder, keep, ""); err != nil {
		return 0, err
	}
	if err := builder.Close(); err != nil {
		return 0, fmt.Errorf("finish keep tar: %w", err)
	}
	return counting.bytes, nil
}

func appendKeepDirRecursive(builder *tar.Writer, keep core.MoveKeep, relative string) error {
	current := gitJoin(keep.Directory(), relative)
	entries, err := os.ReadDir(current)
	if err != nil {
		return fmt.Errorf("read keep directory %s: %w", current, err)
	}
	// os.ReadDir sorts by file name, as the Rust code does.
	for _, entry := range entries {
		entryPath := gitJoin(current, entry.Name())
		entryRelative := entry.Name()
		if relative != "" {
			entryRelative = gitJoin(relative, entry.Name())
		}
		if keep.Skips(entryRelative) {
			continue
		}
		metadata, err := os.Lstat(entryPath)
		if err != nil {
			return fmt.Errorf("lstat %s: %w", entryPath, err)
		}
		switch {
		case checkpointIsSymlink(metadata):
			return fmt.Errorf("refusing symlink inside keep directory: %s", entryRelative)
		case metadata.IsDir():
			header, err := keepTarHeader(metadata, entryRelative, tar.TypeDir)
			if err != nil {
				return fmt.Errorf("append keep dir %s: %w", entryRelative, err)
			}
			if err := builder.WriteHeader(header); err != nil {
				return fmt.Errorf("append keep dir %s: %w", entryRelative, err)
			}
			if err := appendKeepDirRecursive(builder, keep, entryRelative); err != nil {
				return err
			}
		case metadata.Mode().IsRegular():
			file, err := os.Open(entryPath)
			if err != nil {
				return fmt.Errorf("open keep file %s: %w", entryPath, err)
			}
			err = appendKeepFile(builder, file, entryRelative)
			file.Close()
			if err != nil {
				return fmt.Errorf("append keep file %s: %w", entryRelative, err)
			}
		default:
			return fmt.Errorf("refusing unsupported file type inside keep directory: %s", entryRelative)
		}
	}
	return nil
}

// appendKeepFile appends an open regular file as the tar crate's
// `append_file` does: a header from the open file's metadata, then its
// contents.
func appendKeepFile(builder *tar.Writer, file *os.File, relative string) error {
	metadata, err := file.Stat()
	if err != nil {
		return err
	}
	header, err := keepTarHeader(metadata, relative, tar.TypeReg)
	if err != nil {
		return err
	}
	if err := builder.WriteHeader(header); err != nil {
		return err
	}
	_, err = io.CopyN(builder, file, header.Size)
	return err
}

// keepTarHeader is a GNU header as the tar crate's `Header::set_metadata`
// fills it for a keep entry: size (files only), whole-second modification
// time, owner ids and mode (with the file type bits on Unix; 0644/0444 for
// files and 0755 for directories on Windows), and no user or group names.
func keepTarHeader(metadata fs.FileInfo, relative string, typeflag byte) (*tar.Header, error) {
	header, err := tar.FileInfoHeader(gitTarFileInfo{metadata}, "")
	if err != nil {
		return nil, err
	}
	header.Name = filepath.ToSlash(relative)
	header.Typeflag = typeflag
	header.Format = tar.FormatGNU
	header.ModTime = time.Unix(metadata.ModTime().Unix(), 0)
	header.AccessTime, header.ChangeTime = time.Time{}, time.Time{}
	header.Uname, header.Gname = "", ""
	if typeflag == tar.TypeDir {
		header.Size = 0
	}
	if runtime.GOOS == "windows" {
		header.Uid, header.Gid = 0, 0
		switch {
		case typeflag == tar.TypeDir:
			header.Mode = 0o755
		case metadata.Mode().Perm()&0o200 == 0:
			header.Mode = 0o444
		default:
			header.Mode = 0o644
		}
	} else if typeflag == tar.TypeDir {
		header.Mode |= 0o040000
	} else {
		header.Mode |= 0o100000
	}
	return header, nil
}

// ExtractKeepTarStrict extracts a `.mj/keep/` tar archive strictly inside
// dest, refusing any symlink (including dangling symlinks), hard link,
// absolute path, or `..` traversal, and preserving file/directory
// permission bits masked to `0o777`.
func ExtractKeepTarStrict(tarPath string, dest string) error {
	metadata, err := os.Lstat(dest)
	switch {
	case err == nil:
		if checkpointIsSymlink(metadata) {
			return fmt.Errorf("refusing symlink in keep dest: %s", dest)
		}
		if !metadata.IsDir() {
			return fmt.Errorf("keep dest is not a directory: %s", dest)
		}
	case errors.Is(err, fs.ErrNotExist):
		if err := ensureNoSymlinkedKeepAncestors(dest); err != nil {
			return err
		}
		if err := os.MkdirAll(dest, 0o777); err != nil {
			return fmt.Errorf("create keep dest %s: %w", dest, err)
		}
		if err := ensureNoSymlinkedKeepAncestors(dest); err != nil {
			return err
		}
	default:
		return fmt.Errorf("lstat keep dest %s: %w", dest, err)
	}

	file, err := os.Open(tarPath)
	if err != nil {
		return fmt.Errorf("open received keep tar %s: %w", tarPath, err)
	}
	defer file.Close()
	archive := tar.NewReader(file)
	type deferredMode struct {
		path string
		mode os.FileMode
	}
	var deferredDirModes []deferredMode

	for {
		header, err := archive.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return fmt.Errorf("read keep tar entry: %w", err)
		}
		isFile := gitTarIsFile(header)
		isDir := header.Typeflag == tar.TypeDir
		if !isFile && !isDir {
			return fmt.Errorf("refusing non-file/non-directory entry in keep tar: %s", keepTarEntryTypeDebug(header.Typeflag))
		}
		mode := os.FileMode(header.Mode) & 0o777

		rel, err := core.NormalizeKeepPath(keepTarEntryPath(header.Name))
		if err != nil {
			return err
		}
		target := gitJoin(dest, rel)

		// Verify every intermediate component inside dest via Lstat (never
		// a Stat-based existence check, which is false for dangling
		// symlinks).
		cursor := dest
		if parent, ok := checkpointParent(rel); ok {
			for _, component := range checkpointComponents(parent) {
				if component.kind != checkpointComponentNormal {
					continue
				}
				cursor = gitJoin(cursor, component.text)
				metadata, err := os.Lstat(cursor)
				switch {
				case err == nil:
					if checkpointIsSymlink(metadata) {
						return fmt.Errorf("refusing symlink in keep path: %s", cursor)
					}
					if !metadata.IsDir() {
						return fmt.Errorf("keep parent is not a directory: %s", cursor)
					}
				case errors.Is(err, fs.ErrNotExist):
					if err := os.Mkdir(cursor, 0o777); err != nil {
						return fmt.Errorf("create keep subdir %s: %w", cursor, err)
					}
				default:
					return fmt.Errorf("lstat keep subdir %s: %w", cursor, err)
				}
			}
		}

		if isDir {
			metadata, err := os.Lstat(target)
			switch {
			case err == nil:
				if checkpointIsSymlink(metadata) {
					return fmt.Errorf("refusing symlink in keep path: %s", target)
				}
				if !metadata.IsDir() {
					return fmt.Errorf("keep dir target is not a directory: %s", target)
				}
			case errors.Is(err, fs.ErrNotExist):
				if err := os.Mkdir(target, 0o777); err != nil {
					return fmt.Errorf("create keep dir %s: %w", target, err)
				}
			default:
				return fmt.Errorf("lstat keep dir %s: %w", target, err)
			}
			if runtime.GOOS != "windows" {
				// Temporarily keep owner rwx so children inside this
				// directory can be extracted; exact `mode & 0o777` is
				// restored bottom-up after all entries are extracted.
				if err := os.Chmod(target, mode|0o700); err != nil {
					return fmt.Errorf("chmod keep dir %s: %w", target, err)
				}
				deferredDirModes = append(deferredDirModes, deferredMode{target, mode})
			}
			continue
		}
		// Check Lstat directly so dangling symlinks are caught too.
		metadata, err := os.Lstat(target)
		switch {
		case err == nil:
			if checkpointIsSymlink(metadata) {
				return fmt.Errorf("refusing symlink in keep path: %s", target)
			}
			if !metadata.Mode().IsRegular() {
				return fmt.Errorf("keep file target is not a regular file: %s", target)
			}
		case errors.Is(err, fs.ErrNotExist):
		default:
			return fmt.Errorf("lstat keep file %s: %w", target, err)
		}
		out, err := openKeepFileNofollow(target, mode)
		if err != nil {
			return err
		}
		if _, err := io.Copy(out, archive); err != nil {
			out.Close()
			return fmt.Errorf("extract keep file %s: %w", target, err)
		}
		if runtime.GOOS != "windows" {
			if err := out.Chmod(mode); err != nil {
				out.Close()
				return fmt.Errorf("chmod keep file %s: %w", target, err)
			}
		}
		if err := out.Close(); err != nil {
			return fmt.Errorf("extract keep file %s: %w", target, err)
		}
	}

	// Restore restrictive directory modes bottom-up (deepest first).
	slices.SortStableFunc(deferredDirModes, func(left, right deferredMode) int {
		return len(checkpointComponents(right.path)) - len(checkpointComponents(left.path))
	})
	for _, entry := range deferredDirModes {
		if err := os.Chmod(entry.path, entry.mode); err != nil {
			return fmt.Errorf("restore keep dir mode %s: %w", entry.path, err)
		}
	}
	return nil
}

// keepTarEntryPath is the tar crate's `Entry::path` on a header name: a
// trailing slash does not make a component, and on Windows `/` separates
// components as `\` does.
func keepTarEntryPath(name string) string {
	if runtime.GOOS == "windows" {
		return strings.ReplaceAll(name, "/", `\`)
	}
	return name
}

// keepTarEntryTypeDebug is the tar crate's Debug text for an entry type.
func keepTarEntryTypeDebug(typeflag byte) string {
	switch typeflag {
	case tar.TypeReg, '\x00':
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
	return fmt.Sprintf("__Nonexhaustive(%d)", typeflag)
}
