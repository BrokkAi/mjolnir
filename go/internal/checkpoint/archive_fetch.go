// Port of mj-checkpoint/src/archive/fetch.rs.

package checkpoint

import (
	"archive/zip"
	"bufio"
	"bytes"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"lukechampine.com/blake3"
)

// FetchMode is what a caller asked of a path.
type FetchMode int

const (
	// FetchModeContent: a file's bytes, or a zip archive of a directory.
	FetchModeContent FetchMode = iota
	// FetchModeListing: a SessionListing as JSON: a directory's entries, or
	// the file itself.
	FetchModeListing
)

// FetchedKind is what a fetch returned.
type FetchedKind string

const (
	// FetchedKindFile: a regular file, returned as it is.
	FetchedKindFile FetchedKind = "file"
	// FetchedKindDirectory: a directory, returned as a zip archive of
	// everything below it.
	FetchedKindDirectory FetchedKind = "directory"
	// FetchedKindListing: a SessionListing as JSON.
	FetchedKindListing FetchedKind = "listing"
)

func (k *FetchedKind) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch FetchedKind(text) {
	case FetchedKindFile, FetchedKindDirectory, FetchedKindListing:
		*k = FetchedKind(text)
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected one of `file`, `directory`, `listing`", text)
}

// SessionListing is the entries at a path on a session's target.
type SessionListing struct {
	// Path is the path on the target after symlinks were resolved.
	Path string `json:"path"`
	// Entries are a directory's entries in name order, or the one file the
	// path names.
	Entries []ListedEntry `json:"entries"`
}

// MarshalJSON writes nil Entries as [], as serde writes an empty Vec.
func (l SessionListing) MarshalJSON() ([]byte, error) {
	type plain SessionListing
	value := plain(l)
	if value.Entries == nil {
		value.Entries = []ListedEntry{}
	}
	return fetchMarshal(value)
}

func (l *SessionListing) UnmarshalJSON(data []byte) error {
	type plain SessionListing
	return fetchDecodeRequired(data, (*plain)(l), "path", "entries")
}

type ListedEntry struct {
	Name string     `json:"name"`
	Kind ListedKind `json:"kind"`
	// Size is the entry's own size in bytes; a symlink's is the length of
	// its target.
	Size uint64 `json:"size"`
	// Modified is the last modification, as RFC 3339 in UTC, when the target
	// records one.
	Modified *string `json:"modified"`
	// Target is where a symlink points. Links are listed, never followed.
	Target *string `json:"target,omitempty"`
}

func (e *ListedEntry) UnmarshalJSON(data []byte) error {
	type plain ListedEntry
	return fetchDecodeRequired(data, (*plain)(e), "name", "kind", "size")
}

type ListedKind string

const (
	ListedKindFile      ListedKind = "file"
	ListedKindDirectory ListedKind = "directory"
	ListedKindSymlink   ListedKind = "symlink"
	// ListedKindOther: a socket, FIFO or device.
	ListedKindOther ListedKind = "other"
)

func (k *ListedKind) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch ListedKind(text) {
	case ListedKindFile, ListedKindDirectory, ListedKindSymlink, ListedKindOther:
		*k = ListedKind(text)
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected one of `file`, `directory`, `symlink`, `other`", text)
}

// FetchedPath is one path read from a session's target by FetchSessionPath.
type FetchedPath struct {
	Kind FetchedKind
	// Path is the path on the target after symlinks were resolved.
	Path    string
	payload *os.File
	length  uint64
	offset  uint64
}

// SavedFetch is a fetched copy on the calling machine, without its content
// in the response.
type SavedFetch struct {
	Path   string      `json:"path"`
	Kind   FetchedKind `json:"kind"`
	Source string      `json:"source"`
	Bytes  uint64      `json:"bytes"`
}

func (s *SavedFetch) UnmarshalJSON(data []byte) error {
	type plain SavedFetch
	return fetchDecodeRequired(data, (*plain)(s), "path", "kind", "source", "bytes")
}

type fetchedHeader struct {
	Kind   FetchedKind `json:"kind"`
	Path   string      `json:"path"`
	Length *uint64     `json:"length,omitempty"`
}

func (h *fetchedHeader) UnmarshalJSON(data []byte) error {
	type plain fetchedHeader
	return fetchDecodeRequired(data, (*plain)(h), "kind", "path")
}

// LocalPath keeps the source's whole path below `<temp>/mjolnir/<session>`
// so equal filenames from different paths or sessions cannot overwrite each
// other.
func (p *FetchedPath) LocalPath(temp, session string) (string, error) {
	return localFetchPath(p.Kind, p.Path, temp, session)
}

// FetchedPathFromFile is Rust's FetchedPath::from_file. It takes ownership
// of file, closing it on failure.
func FetchedPathFromFile(kind FetchedKind, path string, file *os.File) (*FetchedPath, error) {
	info, err := file.Stat()
	if err != nil {
		file.Close()
		return nil, fmt.Errorf("inspect fetched payload: %w", err)
	}
	return &FetchedPath{Kind: kind, Path: path, payload: file, length: uint64(info.Size())}, nil
}

// FetchedPathFromReader is Rust's FetchedPath::from_reader. It takes
// ownership of file, closing it on failure.
func FetchedPathFromReader(kind FetchedKind, path string, reader io.Reader, file *os.File) (*FetchedPath, error) {
	if _, err := io.Copy(file, reader); err != nil {
		file.Close()
		return nil, fmt.Errorf("spool fetched payload: %w", err)
	}
	return FetchedPathFromFile(kind, path, file)
}

func (p *FetchedPath) Len() uint64 {
	return p.length
}

func (p *FetchedPath) IsEmpty() bool {
	return p.length == 0
}

// Reader reads the payload from its start. Readers have independent
// positions even though their file handle is shared.
func (p *FetchedPath) Reader() *io.SectionReader {
	return io.NewSectionReader(p.payload, int64(p.offset), int64(p.length))
}

// Close releases the payload file, which every Reader shares. (Rust closes it
// when the last clone drops.)
func (p *FetchedPath) Close() error {
	return p.payload.Close()
}

func (p *FetchedPath) Save(temp, session string) (SavedFetch, error) {
	return FetchedPathStore(p.Kind, p.Path, temp, session, p.length, p.Reader())
}

// FetchedPathStore stages on the destination filesystem and publishes only a
// complete transfer. (Rust's FetchedPath::store.)
func FetchedPathStore(kind FetchedKind, source, temp, session string, expected uint64, reader io.Reader) (SavedFetch, error) {
	path, err := localFetchPath(kind, source, temp, session)
	if err != nil {
		return SavedFetch{}, err
	}
	parent := filepath.Dir(path)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return SavedFetch{}, fmt.Errorf("create fetch destination: %w", err)
	}
	// One lock owns this destination's staging path, including across
	// killed worker processes. Never remove the lock inode: another owner
	// may already be waiting on it. Unrelated destinations stay concurrent.
	keyHash := blake3.Sum256([]byte(path))
	key := hex.EncodeToString(keyHash[:])
	lockFile, err := os.OpenFile(filepath.Join(parent, ".mj-fetch-"+key+".lock"), os.O_WRONLY|os.O_CREATE, 0o666)
	if err != nil {
		return SavedFetch{}, fmt.Errorf("open fetch destination lock: %w", err)
	}
	defer lockFile.Close()
	if err := fetchLockExclusive(lockFile); err != nil {
		return SavedFetch{}, fmt.Errorf("lock fetch destination: %w", err)
	}
	lock := core.NewFileLock(lockFile)
	defer lock.Drop()
	stagePath := filepath.Join(parent, ".mj-fetch-"+key+".part")
	// The lock proves the preceding writer has stopped. Reclaim its partial
	// file before reading a retried payload, so restarts cannot accumulate it.
	if err := os.Remove(stagePath); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return SavedFetch{}, fmt.Errorf("reclaim interrupted fetch: %w", err)
	}
	stageFile, err := os.OpenFile(stagePath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return SavedFetch{}, fmt.Errorf("stage fetch destination: %w", err)
	}
	staged := &stagedFetch{file: stageFile, path: stagePath}
	defer staged.drop()
	received, err := io.Copy(staged.file, reader)
	if err != nil {
		return SavedFetch{}, fmt.Errorf("write fetched payload: %w", err)
	}
	if uint64(received) != expected {
		return SavedFetch{}, fmt.Errorf("incomplete fetch transfer: expected %d bytes, received %d", expected, received)
	}
	if err := core.ReachTestHook("fetch_destination_ready"); err != nil {
		return SavedFetch{}, err
	}
	file := staged.file
	staged.file = nil
	err = file.Sync()
	file.Close()
	if err != nil {
		return SavedFetch{}, fmt.Errorf("sync fetched payload: %w", err)
	}
	if err := os.Rename(staged.path, path); err != nil {
		return SavedFetch{}, fmt.Errorf("publish fetched payload: %w", err)
	}
	if err := core.SyncDirectory(parent); err != nil {
		return SavedFetch{}, err
	}
	return SavedFetch{Path: path, Kind: kind, Source: source, Bytes: uint64(received)}, nil
}

// WriteTo writes the wire form: metadata framing is unchanged, a JSON line
// followed by payload bytes. It returns the bytes written.
func (p *FetchedPath) WriteTo(writer io.Writer) (int64, error) {
	length := p.length
	header, err := fetchMarshal(fetchedHeader{Kind: p.Kind, Path: p.Path, Length: &length})
	if err != nil {
		return 0, err
	}
	written, err := writer.Write(append(header, '\n'))
	if err != nil {
		return int64(written), err
	}
	copied, err := io.Copy(writer, p.Reader())
	total := int64(written) + copied
	if err != nil {
		return total, fmt.Errorf("stream fetched payload: %w", err)
	}
	if flusher, ok := writer.(interface{ Flush() error }); ok {
		if err := flusher.Flush(); err != nil {
			return total, fmt.Errorf("flush fetched payload: %w", err)
		}
	}
	return total, nil
}

// FetchedPathReadFrom keeps the wire payload in its original anonymous file.
// The metadata line is a prefix, not another copy of the artifact to
// allocate or rewrite. (Rust's FetchedPath::read_from.) It takes ownership
// of file, closing it on failure.
func FetchedPathReadFrom(file *os.File) (*FetchedPath, error) {
	fetched, err := fetchedPathReadFrom(file)
	if err != nil {
		file.Close()
	}
	return fetched, err
}

func fetchedPathReadFrom(file *os.File) (*FetchedPath, error) {
	if _, err := file.Seek(0, io.SeekStart); err != nil {
		return nil, err
	}
	line, err := bufio.NewReader(file).ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return nil, err
	}
	if line == "" {
		return nil, errors.New("the fetched path has no header line")
	}
	if !utf8.ValidString(line) {
		return nil, errors.New("stream did not contain valid UTF-8")
	}
	var header fetchedHeader
	if err := json.Unmarshal([]byte(line), &header); err != nil {
		return nil, fmt.Errorf("decode fetched path header: %w", err)
	}
	offset := uint64(len(line))
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if uint64(info.Size()) < offset {
		return nil, errors.New("fetch header exceeds payload")
	}
	length := uint64(info.Size()) - offset
	if header.Length != nil && length != *header.Length {
		return nil, fmt.Errorf("incomplete fetch transfer: expected %d bytes, received %d", *header.Length, length)
	}
	return &FetchedPath{Kind: header.Kind, Path: header.Path, payload: file, length: length, offset: offset}, nil
}

// CreateFetchSpool creates an anonymous transfer spool file. Internal
// transfer spools belong to the instance's data filesystem, not `/tmp`,
// which is commonly tmpfs. The final user copy keeps its requested
// destination; only rebuildable transport scratch lives here.
func CreateFetchSpool() (*os.File, error) {
	directory := filepath.Join(core.DataDir(), "fetch-spool")
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return nil, fmt.Errorf("create fetch spool directory: %w", err)
	}
	file, err := fetchAnonymousFile(directory)
	if err != nil {
		return nil, fmt.Errorf("create fetch spool: %w", err)
	}
	return file, nil
}

// stagedFetch is a partial fetch destination, removed by drop unless it was
// published first.
type stagedFetch struct {
	file *os.File
	path string
}

// drop is Rust's Drop for StagedFetch.
func (s *stagedFetch) drop() {
	if s.file != nil {
		s.file.Close()
		s.file = nil
	}
	if err := os.Remove(s.path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		slog.Warn("could not remove staged fetch", "path", s.path, "error", err)
	}
}

func localFetchPath(kind FetchedKind, source, temp, session string) (string, error) {
	sessionComponents := archivePathComponents(session)
	if len(sessionComponents) != 1 || sessionComponents[0].kind != archiveComponentNormal {
		return "", fmt.Errorf("session id %s cannot name a directory", archiveDebugString(session))
	}
	local := filepath.Join(temp, "mjolnir", session)
	named := false
	for _, component := range archivePathComponents(source) {
		if component.kind == archiveComponentNormal {
			local = filepath.Join(local, component.text)
			named = true
		}
	}
	if !named {
		return "", errors.New("the target answered no path for the fetch")
	}
	if kind == FetchedKindDirectory {
		local += ".zip"
	}
	return local, nil
}

// FetchSessionPath reads a file, zips a directory, or lists either, when it
// lies inside one of roots or, when temp is set, inside this machine's
// temporary directory.
//
// Containment is checked after symlinks are resolved, so neither a `..` nor
// a link can reach the rest of the filesystem. A root that does not exist is
// skipped: nothing can lie inside it. Inside a directory, symlinks are stored
// or listed as links and never followed. A refusal is a SessionExportError
// refusal; any other failure is a SessionExportError failure. It takes
// ownership of payload, closing it on failure.
func FetchSessionPath(path string, roots []string, temp bool, mode FetchMode, payload *os.File) (*FetchedPath, error) {
	fetched, err := fetchSessionPath(path, roots, temp, mode, payload)
	if err != nil {
		// The payload file was handed over; a failed fetch releases it, as
		// Rust drops it.
		payload.Close()
	}
	return fetched, err
}

func fetchSessionPath(path string, roots []string, temp bool, mode FetchMode, payload *os.File) (*FetchedPath, error) {
	allowed := slices.Clone(roots)
	if temp {
		allowed = append(allowed, os.TempDir())
		if runtime.GOOS != "windows" {
			allowed = append(allowed, "/tmp")
		}
	}
	// The temporary directory is usually `/tmp` itself; a refusal names each
	// directory once.
	seen := map[string]struct{}{}
	allowed = slices.DeleteFunc(allowed, func(root string) bool {
		key := archivePathKey(root)
		if _, duplicate := seen[key]; duplicate {
			return true
		}
		seen[key] = struct{}{}
		return false
	})
	var resolvedRoots []string
	for _, root := range allowed {
		resolved, err := archiveCanonicalize(root)
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return nil, SessionExportErrorFailed{fmt.Errorf("resolve %s: %w", root, err)}
		}
		resolvedRoots = append(resolvedRoots, resolved)
	}
	resolved, err := archiveCanonicalize(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, SessionExportErrorRefused(fmt.Sprintf("%s does not exist on the session's target", path))
	}
	if err != nil {
		return nil, SessionExportErrorFailed{fmt.Errorf("resolve %s: %w", path, err)}
	}
	if !slices.ContainsFunc(resolvedRoots, func(root string) bool { return archivePathStartsWith(resolved, root) }) {
		return nil, SessionExportErrorRefused(fmt.Sprintf("%s is outside the directories a session shares: %s",
			path, strings.Join(allowed, ", ")))
	}
	info, err := os.Stat(resolved)
	if err != nil {
		return nil, SessionExportErrorFailed{fmt.Errorf("inspect %s: %w", resolved, err)}
	}
	display := strings.ToValidUTF8(resolved, "�")
	var kind FetchedKind
	switch {
	case mode == FetchModeListing:
		if err := listPath(resolved, info, payload); err != nil {
			return nil, err
		}
		kind = FetchedKindListing
	case info.Mode().IsRegular():
		file, err := os.Open(resolved)
		if err != nil {
			return nil, SessionExportErrorFailed{fmt.Errorf("open %s: %w", display, err)}
		}
		_, err = io.Copy(payload, file)
		file.Close()
		if err != nil {
			return nil, SessionExportErrorFailed{fmt.Errorf("read %s: %w", display, err)}
		}
		kind = FetchedKindFile
	case info.IsDir():
		if err := zipDirectory(resolved, payload); err != nil {
			return nil, err
		}
		kind = FetchedKindDirectory
	default:
		return nil, SessionExportErrorRefused(fmt.Sprintf("%s is neither a regular file nor a directory", display))
	}
	fetched, err := FetchedPathFromFile(kind, display, payload)
	if err != nil {
		return nil, SessionExportErrorFailed{err}
	}
	return fetched, nil
}

// listPath writes a SessionListing of path as JSON.
func listPath(path string, info fs.FileInfo, writer io.Writer) error {
	display := strings.ToValidUTF8(path, "�")
	var entries []ListedEntry
	if info.IsDir() {
		children, err := os.ReadDir(path)
		if err != nil {
			return SessionExportErrorFailed{fmt.Errorf("list %s: %w", display, err)}
		}
		// os.ReadDir sorts by name, byte for byte, as Rust sorts OsStrings.
		for _, child := range children {
			childPath := filepath.Join(path, child.Name())
			childInfo, err := os.Lstat(childPath)
			if err != nil {
				return SessionExportErrorFailed{fmt.Errorf("inspect %s: %w", childPath, err)}
			}
			entry, err := listedEntry(childPath, strings.ToValidUTF8(child.Name(), "�"), childInfo)
			if err != nil {
				return SessionExportErrorFailed{err}
			}
			entries = append(entries, entry)
		}
	} else {
		name := display
		if components := archivePathComponents(path); len(components) > 0 &&
			components[len(components)-1].kind == archiveComponentNormal {
			name = strings.ToValidUTF8(components[len(components)-1].text, "�")
		}
		entry, err := listedEntry(path, name, info)
		if err != nil {
			return SessionExportErrorFailed{err}
		}
		entries = []ListedEntry{entry}
	}
	encoded, err := fetchMarshal(SessionListing{Path: display, Entries: entries})
	if err == nil {
		_, err = writer.Write(encoded)
	}
	if err != nil {
		return SessionExportErrorFailed{fmt.Errorf("encode the listing: %w", err)}
	}
	return nil
}

func listedEntry(path, name string, info fs.FileInfo) (ListedEntry, error) {
	entry := ListedEntry{Name: name, Size: uint64(info.Size())}
	switch mode := info.Mode(); {
	case mode&fs.ModeSymlink != 0:
		target, err := os.Readlink(path)
		if err != nil {
			return ListedEntry{}, fmt.Errorf("read link %s: %w", path, err)
		}
		target = strings.ToValidUTF8(target, "�")
		entry.Kind, entry.Target = ListedKindSymlink, &target
	case mode.IsDir():
		entry.Kind = ListedKindDirectory
	case mode.IsRegular():
		entry.Kind = ListedKindFile
	default:
		entry.Kind = ListedKindOther
	}
	// RFC 3339 in UTC to the second, as chrono's
	// to_rfc3339_opts(SecondsFormat::Secs, true) writes it.
	modified := info.ModTime().UTC().Format("2006-01-02T15:04:05Z")
	entry.Modified = &modified
	return entry, nil
}

// fetchEntryKind is the kind of a fetchEntry (Rust's private Entry enum).
type fetchEntryKind int

const (
	fetchEntryDirectory fetchEntryKind = iota
	fetchEntryFile
	fetchEntrySymlink
)

// fetchEntry is one entry of a zipped directory: its path relative to the
// directory's parent, and for a symlink, where it points.
type fetchEntry struct {
	name   string
	kind   fetchEntryKind
	target string
}

// collectEntries lists every entry below directory, in name order, keyed by
// its path relative to the directory's parent so the archive unpacks into one
// directory.
func collectEntries(directory, name string, entries *[]fetchEntry) error {
	*entries = append(*entries, fetchEntry{name: name, kind: fetchEntryDirectory})
	children, err := os.ReadDir(directory)
	if err != nil {
		return fmt.Errorf("list %s: %w", directory, err)
	}
	// os.ReadDir sorts by name, byte for byte, as Rust sorts OsStrings.
	for _, child := range children {
		path := filepath.Join(directory, child.Name())
		childName := filepath.Join(name, child.Name())
		info, err := os.Lstat(path)
		if err != nil {
			return fmt.Errorf("inspect %s: %w", path, err)
		}
		switch mode := info.Mode(); {
		case mode&fs.ModeSymlink != 0:
			target, err := os.Readlink(path)
			if err != nil {
				return fmt.Errorf("read link %s: %w", path, err)
			}
			*entries = append(*entries, fetchEntry{name: childName, kind: fetchEntrySymlink, target: target})
		case mode.IsDir():
			if err := collectEntries(path, childName, entries); err != nil {
				return err
			}
		case mode.IsRegular():
			*entries = append(*entries, fetchEntry{name: childName, kind: fetchEntryFile})
		}
		// Sockets, FIFOs and devices have no content to archive.
	}
	return nil
}

func zipDirectory(directory string, output io.Writer) error {
	name := "root"
	if components := archivePathComponents(directory); len(components) > 0 &&
		components[len(components)-1].kind == archiveComponentNormal {
		name = components[len(components)-1].text
	}
	var entries []fetchEntry
	if err := collectEntries(directory, name, &entries); err != nil {
		return SessionExportErrorFailed{err}
	}
	failed := func(err error, entry string) error {
		return SessionExportErrorFailed{fmt.Errorf("archive %s: %w", entry, err)}
	}
	parent := filepath.Dir(directory)
	writer := zip.NewWriter(output)
	for _, entry := range entries {
		// Zip names use `/` whatever the target's separator is.
		var parts []string
		for _, component := range archivePathComponents(entry.name) {
			parts = append(parts, strings.ToValidUTF8(component.text, "�"))
		}
		zipName := strings.Join(parts, "/")
		path := filepath.Join(parent, entry.name)
		switch entry.kind {
		case fetchEntryDirectory:
			if _, err := writer.CreateHeader(archiveFileHeader(zipName+"/", zip.Store, fs.ModeDir|0o755)); err != nil {
				return failed(err, path)
			}
		case fetchEntrySymlink:
			link, err := writer.CreateHeader(archiveFileHeader(zipName, zip.Store, fs.ModeSymlink|0o777))
			if err == nil {
				_, err = io.WriteString(link, strings.ToValidUTF8(entry.target, "�"))
			}
			if err != nil {
				return failed(err, path)
			}
		case fetchEntryFile:
			// archive/zip switches to ZIP64 by itself, which also covers a
			// file that grows past 4 GiB while it is read.
			mode := fs.FileMode(0o644)
			if runtime.GOOS != "windows" {
				info, err := os.Stat(path)
				if err != nil {
					return SessionExportErrorFailed{fmt.Errorf("inspect %s: %w", path, err)}
				}
				mode = info.Mode().Perm()
			}
			content, err := writer.CreateHeader(archiveFileHeader(zipName, zip.Deflate, mode))
			if err != nil {
				return failed(err, path)
			}
			file, err := os.Open(path)
			if err != nil {
				return SessionExportErrorFailed{fmt.Errorf("open %s: %w", path, err)}
			}
			_, err = io.Copy(content, file)
			file.Close()
			if err != nil {
				return SessionExportErrorFailed{fmt.Errorf("archive %s: %w", path, err)}
			}
		}
	}
	if err := writer.Close(); err != nil {
		return failed(err, directory)
	}
	return nil
}

// fetchMarshal encodes v as serde_json writes it: compact, without escaping
// HTML characters.
func fetchMarshal(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// fetchDecodeRequired decodes data into v, a pointer to a struct type without
// its own UnmarshalJSON, after checking that data is an object holding every
// required member, as serde requires fields that have no default.
func fetchDecodeRequired(data []byte, v any, required ...string) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return err
	}
	for _, name := range required {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return json.Unmarshal(data, v)
}
