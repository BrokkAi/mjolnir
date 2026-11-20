// Port of mj-checkpoint/src/archive/fetch.rs.

package checkpoint

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
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

// FetchedPathFromFile is Rust's FetchedPath::from_file.
func FetchedPathFromFile(kind FetchedKind, path string, file *os.File) (*FetchedPath, error) {
	info, err := file.Stat()
	if err != nil {
		return nil, fmt.Errorf("inspect fetched payload: %w", err)
	}
	return &FetchedPath{Kind: kind, Path: path, payload: file, length: uint64(info.Size())}, nil
}

// FetchedPathFromReader is Rust's FetchedPath::from_reader.
func FetchedPathFromReader(kind FetchedKind, path string, reader io.Reader, file *os.File) (*FetchedPath, error) {
	if _, err := io.Copy(file, reader); err != nil {
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
	panic("unported: mj-checkpoint/src/archive/fetch.rs FetchedPath::store")
}

// WriteTo writes the wire form: metadata framing is unchanged, a JSON line
// followed by payload bytes. It returns the bytes written.
func (p *FetchedPath) WriteTo(writer io.Writer) (int64, error) {
	panic("unported: mj-checkpoint/src/archive/fetch.rs FetchedPath::write_to")
}

// FetchedPathReadFrom keeps the wire payload in its original anonymous file.
// The metadata line is a prefix, not another copy of the artifact to
// allocate or rewrite. (Rust's FetchedPath::read_from.)
func FetchedPathReadFrom(file *os.File) (*FetchedPath, error) {
	panic("unported: mj-checkpoint/src/archive/fetch.rs FetchedPath::read_from")
}

// CreateFetchSpool creates an anonymous transfer spool file. Internal
// transfer spools belong to the instance's data filesystem, not `/tmp`,
// which is commonly tmpfs. The final user copy keeps its requested
// destination; only rebuildable transport scratch lives here.
func CreateFetchSpool() (*os.File, error) {
	panic("unported: mj-checkpoint/src/archive/fetch.rs create_fetch_spool")
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
	panic("unported: mj-checkpoint/src/archive/fetch.rs local_fetch_path")
}

// FetchSessionPath reads a file, zips a directory, or lists either, when it
// lies inside one of roots or, when temp is set, inside this machine's
// temporary directory.
//
// Containment is checked after symlinks are resolved, so neither a `..` nor
// a link can reach the rest of the filesystem. A root that does not exist is
// skipped: nothing can lie inside it. Inside a directory, symlinks are stored
// or listed as links and never followed. A refusal is a SessionExportError
// refusal; any other failure is a SessionExportError failure.
func FetchSessionPath(path string, roots []string, temp bool, mode FetchMode, payload *os.File) (*FetchedPath, error) {
	panic("unported: mj-checkpoint/src/archive/fetch.rs fetch_session_path")
}

// listPath writes a SessionListing of path as JSON.
func listPath(path string, info fs.FileInfo, writer io.Writer) error {
	panic("unported: mj-checkpoint/src/archive/fetch.rs list_path")
}

func listedEntry(path, name string, info fs.FileInfo) (ListedEntry, error) {
	panic("unported: mj-checkpoint/src/archive/fetch.rs listed_entry")
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
	panic("unported: mj-checkpoint/src/archive/fetch.rs collect_entries")
}

func zipDirectory(directory string, output io.Writer) error {
	panic("unported: mj-checkpoint/src/archive/fetch.rs zip_directory")
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
