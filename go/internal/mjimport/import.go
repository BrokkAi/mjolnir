// Port of mj-import/src/import.rs.
//
// Import native harness sessions into Hel's durable archive format.
//
// Rust's `import` module and its submodules (`import/*.rs`) are files of this
// package named `import_<submodule>.go`. Rust calls into the `muse`
// submodule by its path (`muse::scan`, `muse::locate`); those functions carry
// a `muse` prefix here (museScan, museLocate, museValidateTranscript,
// museRecentDirectories).
//
// A function whose Go name would equal a type's keeps the type's name for the
// type and takes a `For` suffix: GithubRepositoryFor (Rust's
// github_repository), SessionEditTargetsFor, ImportSafetyIssuesFor,
// claudeNativeSummaryFor, codexSessionMetadataFor, codexSessionSummaryFor,
// kimiSessionSummaryFor and museSummaryFor.

package mjimport

import (
	"bufio"
	"cmp"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// GithubRepository is a repository on GitHub, as its owner and name.
type GithubRepository struct {
	Owner      string
	Repository string
}

// Source is the repository as `owner/repository`.
func (r GithubRepository) Source() string {
	return r.Owner + "/" + r.Repository
}

// GithubRepositoryFromOrigin reads a GitHub repository from an origin,
// accepting setup's `owner/repository` shorthand.
func GithubRepositoryFromOrigin(origin string) *GithubRepository {
	return GithubRepositoryFor(origin, core.ShorthandAllowed)
}

// GithubRepositoryFor is Rust's `github_repository` (renamed: the type
// GithubRepository keeps its name). nil when origin names no GitHub
// repository.
func GithubRepositoryFor(origin string, shorthand core.Shorthand) *GithubRepository {
	remote := core.ParseGithubRemote(origin, shorthand)
	if remote == nil {
		return nil
	}
	return &GithubRepository{Owner: remote.Owner, Repository: remote.Repository}
}

// NativeProjectSeed is only project metadata read from the newest native
// sessions. Never imports conversations, computes checkout sizes, or probes
// Git for this seed.
type NativeProjectSeed struct {
	Directories []string
	// Errors are (path, message) pairs.
	Errors [][2]string
}

// RecentProjectDirectories reads the project directories of the newest
// native sessions in a harness home, at most limit of them.
func RecentProjectDirectories(
	harness core.HarnessKind,
	home string,
	limit int,
	executor core.CommandExecutor,
) (NativeProjectSeed, error) {
	if !importIsDir(home) {
		return NativeProjectSeed{}, fmt.Errorf("profile home is missing: %s", home)
	}
	var files []fileScanCandidate
	switch harness {
	case core.HarnessKindCodex:
		sessions, ok, err := codexIndexedSessionsLimited(home, limit)
		if err != nil {
			return NativeProjectSeed{}, err
		}
		if ok {
			directories := make([]string, 0, len(sessions))
			for _, session := range sessions {
				directories = append(directories, session.Cwd)
			}
			return NativeProjectSeed{Directories: directories, Errors: [][2]string{}}, nil
		}
		titles, err := codexNativeTitles(home)
		if err != nil {
			return NativeProjectSeed{}, err
		}
		candidates := []fileScanCandidate{}
		if err := collectCodexCandidatePaths(filepath.Join(home, "sessions"), 0, titles, &candidates); err != nil {
			return NativeProjectSeed{}, err
		}
		sortFileCandidatesNewestFirst(candidates)
		files = candidates
	case core.HarnessKindClaude:
		candidates, err := claudeCandidates(home)
		if err != nil {
			return NativeProjectSeed{}, err
		}
		files = candidates
	case core.HarnessKindKimi, core.HarnessKindGrok:
		var candidates []kimiScanCandidate
		var err error
		if harness == core.HarnessKindKimi {
			candidates, err = kimiIndexedCandidates(home, filepath.Join(home, "sessions"))
		} else {
			candidates, err = grokCandidates(filepath.Join(home, "sessions"))
		}
		if err != nil {
			return NativeProjectSeed{}, err
		}
		slices.SortStableFunc(candidates, func(a, b kimiScanCandidate) int {
			if order := b.modifiedAt.Compare(a.modifiedAt); order != 0 {
				return order
			}
			return strings.Compare(b.nativeSessionID, a.nativeSessionID)
		})
		directories := []string{}
		for index, candidate := range candidates {
			if index >= limit {
				break
			}
			directories = append(directories, candidate.cwd)
		}
		return NativeProjectSeed{Directories: directories, Errors: [][2]string{}}, nil
	case core.HarnessKindMuse:
		return museRecentDirectories(home, limit, executor)
	case core.HarnessKindOpenCode, core.HarnessKindDsh, core.HarnessKindAntigravity:
		// OpenCode sessions live in a SQLite database that Mjolnir cannot
		// read yet, and no reader of dsh-acp's session records exists yet,
		// so the catalog has no histories to seed from this home.
		return NativeProjectSeed{Directories: []string{}, Errors: [][2]string{}}, nil
	default:
		panic(fmt.Sprintf("unknown harness kind %q", string(harness)))
	}
	seed := NativeProjectSeed{Directories: []string{}, Errors: [][2]string{}}
	for index, candidate := range files {
		if index >= limit {
			break
		}
		if executor.CancellationRequested() {
			return NativeProjectSeed{}, errors.New("project discovery cancelled")
		}
		cwd, err := NativeProjectDirectory(candidate.path, harness, executor)
		if err != nil {
			seed.Errors = append(seed.Errors, [2]string{candidate.path, fmt.Sprintf("%s: %s", candidate.path, err)})
			continue
		}
		seed.Directories = append(seed.Directories, cwd)
	}
	return seed, nil
}

// sortFileCandidatesNewestFirst orders candidates newest first, then by
// path, both descending.
func sortFileCandidatesNewestFirst(candidates []fileScanCandidate) {
	slices.SortStableFunc(candidates, func(a, b fileScanCandidate) int {
		if order := b.modifiedAt.Compare(a.modifiedAt); order != 0 {
			return order
		}
		return importComparePaths(b.path, a.path)
	})
}

// nativeProjectHeaderBytes bounds NativeProjectDirectory's read: project
// metadata lives near the start of each log.
const nativeProjectHeaderBytes = 1024 * 1024

// NativeProjectDirectory reads a native log's project directory. Project
// metadata lives near the start of each log. Bound the read and stop as soon
// as it is found, without parsing the conversation after it.
func NativeProjectDirectory(
	path string,
	harness core.HarnessKind,
	executor core.CommandExecutor,
) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", importIOError(err)
	}
	defer file.Close()
	lines := newImportLineReader(io.LimitReader(file, nativeProjectHeaderBytes))
	for {
		line, ok, err := lines.next()
		if !ok && err == nil {
			break
		}
		if executor.CancellationRequested() {
			return "", errors.New("project discovery cancelled")
		}
		if err != nil {
			return "", err
		}
		if !strings.Contains(line, "cwd") && !strings.Contains(line, "workspace_root") {
			continue
		}
		record, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			return "", err
		}
		var cwd any
		var present bool
		switch harness {
		case core.HarnessKindCodex:
			cwd, present = importJSONPointer(record, "payload", "cwd")
		case core.HarnessKindClaude:
			cwd, present = importJSONGet(record, "cwd")
		case core.HarnessKindMuse:
			cwd, present = importJSONPointer(record, "payload", "record", "workspace_root")
		}
		if text, ok := importJSONStr(cwd, present); ok {
			if !filepath.IsAbs(text) {
				return "", errors.New("native project directory is not absolute")
			}
			return text, nil
		}
	}
	return "", errors.New("native log has no project directory in its metadata header")
}

// importLineReader is Rust's `BufRead::lines`: each line without its "\n"
// or "\r\n", an error for a line that is not UTF-8, and no empty line after
// a final newline.
type importLineReader struct {
	reader *bufio.Reader
}

func newImportLineReader(reader io.Reader) *importLineReader {
	return &importLineReader{reader: bufio.NewReader(reader)}
}

// next is the next line; ok is false at the end of the input.
func (r *importLineReader) next() (string, bool, error) {
	line, err := r.reader.ReadString('\n')
	if err != nil && !errors.Is(err, io.EOF) {
		return "", false, importIOError(err)
	}
	if line == "" && err != nil {
		return "", false, nil
	}
	line = strings.TrimSuffix(line, "\n")
	line = strings.TrimSuffix(line, "\r")
	if !utf8.ValidString(line) {
		return "", false, errors.New("stream did not contain valid UTF-8")
	}
	return line, true, nil
}

// importIOError is err as Rust's io::Error displays it, for the context
// chains anyhow prints: an OS error shows the system's description and its
// number (`No such file or directory (os error 2)`) without Go's operation
// and path prefix. It unwraps to err, so errors.Is and errors.As still see
// the errno.
func importIOError(err error) error {
	if err == nil {
		return nil
	}
	return importRustIOError{err: err}
}

type importRustIOError struct {
	err error
}

func (e importRustIOError) Error() string { return core.IOErrorDisplay(e.err) }

func (e importRustIOError) Unwrap() error { return e.err }

// importIsNotFound is Rust's `error.kind() == ErrorKind::NotFound`.
func importIsNotFound(err error) bool {
	return errors.Is(err, os.ErrNotExist) || errors.Is(err, syscall.ENOENT)
}

// The importJSON helpers read a value decoded by jsonvalue.Parse as
// serde_json's Value accessors do.

// importJSONGet is `Value::get(key)`: the member of an object, absent for
// any other value.
func importJSONGet(value any, key string) (any, bool) {
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, false
	}
	return object.Get(key)
}

// importJSONPointer is `Value::pointer` for a path of object keys and array
// indices.
func importJSONPointer(value any, path ...string) (any, bool) {
	for _, step := range path {
		switch current := value.(type) {
		case jsonvalue.Object:
			next, ok := current.Get(step)
			if !ok {
				return nil, false
			}
			value = next
		case []any:
			if step == "" || step[0] == '+' || (len(step) > 1 && step[0] == '0') {
				return nil, false
			}
			index, err := strconv.ParseUint(step, 10, 64)
			if err != nil || index >= uint64(len(current)) {
				return nil, false
			}
			value = current[index]
		default:
			return nil, false
		}
	}
	return value, true
}

// importJSONStr is `Value::as_str` on an optional value.
func importJSONStr(value any, present bool) (string, bool) {
	text, ok := value.(string)
	return text, present && ok
}

// importJSONBool is `Value::as_bool` on an optional value.
func importJSONBool(value any, present bool) (bool, bool) {
	flag, ok := value.(bool)
	return flag, present && ok
}

// importJSONIndex is serde_json's `value[key]`: the member, or null.
func importJSONIndex(value any, key string) any {
	member, _ := importJSONGet(value, key)
	return member
}

// importNonblank is a string value that is not blank once trimmed.
func importNonblank(value any, present bool) (string, bool) {
	text, ok := importJSONStr(value, present)
	if !ok || strings.TrimSpace(text) == "" {
		return "", false
	}
	return text, true
}

// importHarnessDebug is HarnessKind's Rust Debug name.
func importHarnessDebug(kind core.HarnessKind) string {
	switch kind {
	case core.HarnessKindCodex:
		return "Codex"
	case core.HarnessKindClaude:
		return "Claude"
	case core.HarnessKindKimi:
		return "Kimi"
	case core.HarnessKindGrok:
		return "Grok"
	case core.HarnessKindMuse:
		return "Muse"
	case core.HarnessKindOpenCode:
		return "OpenCode"
	case core.HarnessKindDsh:
		return "Dsh"
	case core.HarnessKindAntigravity:
		return "Antigravity"
	}
	return string(kind)
}

// importIsFile is Rust's `Path::is_file`: the path, following symlinks, is
// a regular file.
func importIsFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

// importIsDir is Rust's `Path::is_dir`: the path, following symlinks, is a
// directory.
func importIsDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}

// importExists is Rust's `Path::exists`: the path, following symlinks, can
// be read.
func importExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// importCanonicalize is Rust's `fs::canonicalize`: an absolute path with
// every link resolved, which must exist. `..` steps out of the directory a
// link resolved to, as realpath does.
func importCanonicalize(path string) (string, error) {
	if path == "" {
		// realpath("") fails rather than naming the working directory.
		return "", importIOError(&os.PathError{Op: "canonicalize", Path: path, Err: syscall.ENOENT})
	}
	if !filepath.IsAbs(path) {
		directory, err := os.Getwd()
		if err != nil {
			return "", importIOError(err)
		}
		path = directory + string(filepath.Separator) + path
	}
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return "", importIOError(err)
	}
	return resolved, nil
}

// importCanonicalizeOr is `fs::canonicalize(path).unwrap_or(path)`.
func importCanonicalizeOr(path string) string {
	if canonical, err := importCanonicalize(path); err == nil {
		return canonical
	}
	return path
}

type importComponentKind int

// The kinds in Rust's Component order, which its derived Ord follows.
const (
	importComponentPrefix importComponentKind = iota
	importComponentRootDir
	importComponentCurDir
	importComponentParentDir
	importComponentNormal
)

// importPathComponent is one component of a path as Rust's
// Path::components yields it; text is the component as written.
type importPathComponent struct {
	kind importComponentKind
	text string
}

// importPathComponents splits path as Rust's Path::components does on this
// platform: repeated separators and interior "." components vanish, a
// leading "." is CurDir, and a root or a Windows prefix is its own
// component.
func importPathComponents(path string) []importPathComponent {
	windows := runtime.GOOS == "windows"
	isSeparator := func(c byte) bool { return c == '/' || windows && c == '\\' }
	var components []importPathComponent
	rest := path
	hasRoot := false
	if windows {
		if prefix := filepath.VolumeName(rest); prefix != "" {
			components = append(components, importPathComponent{importComponentPrefix, prefix})
			rest = rest[len(prefix):]
			// A UNC or device prefix is rooted without a separator of its own.
			hasRoot = len(prefix) > 2
		}
	}
	if rest != "" && isSeparator(rest[0]) {
		components = append(components, importPathComponent{importComponentRootDir, string(filepath.Separator)})
		hasRoot = true
		rest = rest[1:]
	} else if !hasRoot && rest != "" && rest[0] == '.' && (len(rest) == 1 || isSeparator(rest[1])) {
		// Only a leading "." of a path without a root is a component.
		components = append(components, importPathComponent{importComponentCurDir, "."})
		rest = rest[1:]
	}
	for rest != "" {
		end := 0
		for end < len(rest) && !isSeparator(rest[end]) {
			end++
		}
		part := rest[:end]
		rest = rest[end:]
		if rest != "" {
			rest = rest[1:]
		}
		switch part {
		case "", ".":
		case "..":
			components = append(components, importPathComponent{importComponentParentDir, ".."})
		default:
			components = append(components, importPathComponent{importComponentNormal, part})
		}
	}
	return components
}

// importJoinComponents pushes each component onto an empty PathBuf.
func importJoinComponents(components []importPathComponent) string {
	joined := ""
	for _, component := range components {
		switch {
		case component.kind == importComponentPrefix || component.kind == importComponentRootDir:
			joined += component.text
		case joined == "" || os.IsPathSeparator(joined[len(joined)-1]):
			joined += component.text
		default:
			joined += string(filepath.Separator) + component.text
		}
	}
	return joined
}

// importPathStartsWith is Rust's `Path::starts_with`: whether base is a
// component prefix of path.
func importPathStartsWith(path, base string) bool {
	pathComponents, baseComponents := importPathComponents(path), importPathComponents(base)
	if len(baseComponents) > len(pathComponents) {
		return false
	}
	for index, component := range baseComponents {
		if pathComponents[index] != component {
			return false
		}
	}
	return true
}

// importPathStripPrefix is Rust's `Path::strip_prefix`: path relative to
// base, when base is a component prefix of it.
func importPathStripPrefix(path, base string) (string, bool) {
	if !importPathStartsWith(path, base) {
		return "", false
	}
	return importJoinComponents(importPathComponents(path)[len(importPathComponents(base)):]), true
}

// importPathParent is Rust's `Path::parent`: false for a root or an empty
// path, and "" for a single relative component.
func importPathParent(path string) (string, bool) {
	components := importPathComponents(path)
	if len(components) == 0 {
		return "", false
	}
	switch components[len(components)-1].kind {
	case importComponentNormal, importComponentCurDir, importComponentParentDir:
		return importJoinComponents(components[:len(components)-1]), true
	}
	return "", false
}

// importPathFileName is Rust's `Path::file_name`: the final normal
// component, false when the path ends in `..` or is a root.
func importPathFileName(path string) (string, bool) {
	components := importPathComponents(path)
	if len(components) == 0 {
		return "", false
	}
	last := components[len(components)-1]
	if last.kind != importComponentNormal {
		return "", false
	}
	return last.text, true
}

// importPathFileStem is Rust's `Path::file_stem`: the file name without its
// final extension, the whole name when its only dot leads it.
func importPathFileStem(path string) (string, bool) {
	name, ok := importPathFileName(path)
	if !ok {
		return "", false
	}
	before, _, _ := importSplitFileAtDot(name)
	return before, true
}

// importPathExtension is Rust's `Path::extension`.
func importPathExtension(path string) (string, bool) {
	name, ok := importPathFileName(path)
	if !ok {
		return "", false
	}
	_, after, extension := importSplitFileAtDot(name)
	return after, extension
}

// importSplitFileAtDot is Rust's `rsplit_file_at_dot`: the name before and
// after its last dot, with no extension for `..` or a name whose only dot
// leads it.
func importSplitFileAtDot(name string) (before, after string, extension bool) {
	if name == ".." {
		return name, "", false
	}
	index := strings.LastIndexByte(name, '.')
	if index <= 0 {
		return name, "", false
	}
	return name[:index], name[index+1:], true
}

// importComparePaths is Rust's `Ord for Path`: component by component.
func importComparePaths(a, b string) int {
	return slices.CompareFunc(importPathComponents(a), importPathComponents(b),
		func(left, right importPathComponent) int {
			// Component's derived order: by kind, then by text.
			if order := cmp.Compare(left.kind, right.kind); order != 0 {
				return order
			}
			return strings.Compare(left.text, right.text)
		})
}
