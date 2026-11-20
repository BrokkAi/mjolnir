// Port of mj-checkpoint/src/archive/git/operation.rs.
//
// Capture and restore of an unfinished Git operation in an independent
// clone.
//
// A clone stopped in a merge, cherry-pick, revert, rebase or bisect keeps the
// operation in three places: unmerged index entries, the working-tree files
// of the unmerged paths, and state files in the Git directory. A patch can
// carry none of them, so the operation travels beside the snapshot's patches
// and is written back after the rest of the clone is restored.

package checkpoint

import (
	"archive/tar"
	"bytes"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// stateFiles are the Git-directory files an unfinished operation keeps. Each
// is found with `git rev-parse --git-path`, which knows which ones belong to
// the worktree and which to the repository. The names are those Git's
// merge, sequencer, am, bisect and rerere code write.
var stateFiles = [20]string{
	"AUTO_MERGE",
	"BISECT_ANCESTORS_OK",
	"BISECT_EXPECTED_REV",
	"BISECT_FIRST_PARENT",
	"BISECT_HEAD",
	"BISECT_LOG",
	"BISECT_NAMES",
	"BISECT_RUN",
	"BISECT_START",
	"BISECT_TERMS",
	"CHERRY_PICK_HEAD",
	"MERGE_AUTOSTASH",
	"MERGE_HEAD",
	"MERGE_MODE",
	"MERGE_MSG",
	"MERGE_RR",
	"ORIG_HEAD",
	"REBASE_HEAD",
	"REVERT_HEAD",
	"SQUASH_MSG",
}

// stateDirectories are the Git-directory directories an unfinished operation
// keeps, copied whole.
var stateDirectories = [4]string{"rebase-apply", "rebase-merge", "rr-cache", "sequencer"}

// inProgressMarkers are the state entries whose presence means an operation
// is in progress. The others, such as `ORIG_HEAD` and `rr-cache`, outlive
// the operation that wrote them, so they are kept only alongside one.
var inProgressMarkers = [9]string{
	"BISECT_START",
	"CHERRY_PICK_HEAD",
	"MERGE_HEAD",
	"REBASE_HEAD",
	"REVERT_HEAD",
	"SQUASH_MSG",
	"rebase-apply",
	"rebase-merge",
	"sequencer",
}

// operationRefNamespaces are ref namespaces only an operation in progress
// writes: bisect's good and bad marks, and the labels of
// `git rebase --rebase-merges`.
var operationRefNamespaces = [2]string{"refs/bisect/", "refs/rewritten/"}

// todoLists are todo lists, whose instructions may name commits by
// abbreviated ids.
var todoLists = [4]string{
	"rebase-merge/done",
	"rebase-merge/git-rebase-todo",
	"rebase-merge/git-rebase-todo.backup",
	"sequencer/todo",
}

// rerereCache is the state directory that holds file contents rather than
// object names.
const rerereCache = "rr-cache"

// collectedOperation is an unfinished operation read from a clone, ready to
// archive.
type collectedOperation struct {
	metadata core.SavedGitOperation
	payloads GitOperationPayloads
	// commits, tags and contents are the objects the operation names, by
	// type, for the temporary refs that put them in the committed bundle.
	// contents holds (type, object) pairs of trees and blobs.
	commits  []string
	tags     []string
	contents [][2]string
}

// unmergedPaths are the unmerged paths, each once, in index order. The
// staged and unstaged patches leave them out.
func (o *collectedOperation) unmergedPaths() []string {
	return uniquePaths(o.metadata.Unmerged)
}

// carriesTags reports whether a carried object is an annotated tag, which a
// bundle must hold even when the commit it names is already on the base.
func (o *collectedOperation) carriesTags() bool {
	return len(o.tags) > 0
}

// carryObjects points temporary refs below `prefix` at the objects the
// operation names, so the committed bundle carries them, and adds each ref
// to `refs` as soon as it exists so the caller removes it on every path.
//
// A bundle can only carry what refs reach. Commits become parents of one
// wrapper commit, whose tree holds the trees and blobs (an `AUTO_MERGE`
// tree, the stages of a conflict); annotated tags each get a ref.
func (o *collectedOperation) carryObjects(runner GitCommandRunner, repository string, prefix string, refs *[]string) error {
	for index, tag := range o.tags {
		name := fmt.Sprintf("%s/tag/%08d", prefix, index)
		if _, err := gitBytes(runner, repository, []string{"update-ref", name, tag}, nil,
			"preserve a Git operation tag for the checkpoint bundle"); err != nil {
			return err
		}
		*refs = append(*refs, name)
	}
	if len(o.commits) == 0 && len(o.contents) == 0 {
		return nil
	}
	var listing strings.Builder
	for index, content := range o.contents {
		kind, object := content[0], content[1]
		mode := "100644"
		if kind == "tree" {
			mode = "040000"
		}
		fmt.Fprintf(&listing, "%s %s %s\t%08d\n", mode, kind, object, index)
	}
	treeOutput, err := gitBytes(runner, repository, []string{"mktree"}, []byte(listing.String()),
		"record the Git operation's trees and blobs")
	if err != nil {
		return err
	}
	tree, err := trimOutput(treeOutput, "decode the Git operation tree id")
	if err != nil {
		return err
	}
	arguments := []string{"commit-tree", tree, "-m", "Mjolnir checkpoint: objects of an unfinished Git operation"}
	for _, commit := range o.commits {
		arguments = append(arguments, "-p", commit)
	}
	// A clone need not have an identity configured, and a fixed one keeps
	// the wrapper the same for the same operation.
	identity := [][2]string{
		{"GIT_AUTHOR_NAME", "Mjolnir"},
		{"GIT_AUTHOR_EMAIL", "mjolnir@localhost"},
		{"GIT_AUTHOR_DATE", "@0 +0000"},
		{"GIT_COMMITTER_NAME", "Mjolnir"},
		{"GIT_COMMITTER_EMAIL", "mjolnir@localhost"},
		{"GIT_COMMITTER_DATE", "@0 +0000"},
	}
	wrapperOutput, err := gitSuccess(runner, repository, &GitCommand{Arguments: arguments, Env: identity},
		"record the Git operation's commits")
	if err != nil {
		return err
	}
	wrapper, err := trimOutput(wrapperOutput, "decode the Git operation commit id")
	if err != nil {
		return err
	}
	name := prefix + "/objects"
	if _, err := gitBytes(runner, repository, []string{"update-ref", name, wrapper}, nil,
		"preserve the Git operation's objects for the checkpoint bundle"); err != nil {
		return err
	}
	*refs = append(*refs, name)
	return nil
}

func uniquePaths(entries []core.SavedUnmergedEntry) []string {
	var paths []string
	for _, entry := range entries {
		if len(paths) == 0 || paths[len(paths)-1] != entry.Path {
			paths = append(paths, entry.Path)
		}
	}
	return paths
}

// operationCollect reads the operation `repository` is in the middle of, or
// nil when it has no unmerged entries and no operation in progress. nil
// leaves the snapshot exactly as it was before operations were captured.
// (Rust: operation::collect.)
func operationCollect(runner GitCommandRunner, repository string) (*collectedOperation, error) {
	listed, err := gitBytes(runner, repository, []string{"ls-files", "--unmerged", "-z"}, nil,
		"inspect clone merge conflicts")
	if err != nil {
		return nil, err
	}
	conflicts, err := unmergedPaths(listed)
	if err != nil {
		return nil, err
	}
	var unmerged []core.SavedUnmergedEntry
	for _, path := range conflicts {
		relative, err := core.FromGitBytes([]byte(path.path))
		if err != nil {
			return nil, err
		}
		if err := validateWorktreeRelativePath(relative); err != nil {
			return nil, err
		}
		for index, stage := range path.stages {
			if stage != nil {
				unmerged = append(unmerged, core.SavedUnmergedEntry{
					Path:   relative,
					Stage:  uint8(index + 1),
					Mode:   stage.mode,
					Object: stage.object,
				})
			}
		}
	}
	names := slices.Concat(stateFiles[:], stateDirectories[:])
	locations, err := gitPaths(runner, repository, names)
	if err != nil {
		return nil, err
	}
	inProgress := false
	for _, location := range locations {
		if !slices.Contains(inProgressMarkers[:], location[0]) {
			continue
		}
		exists, err := entryExists(location[1])
		if err != nil {
			return nil, err
		}
		inProgress = inProgress || exists
	}
	if len(unmerged) == 0 && !inProgress {
		return nil, nil
	}

	paths := uniquePaths(unmerged)
	if err := refuseIndexEntriesBelow(runner, repository, paths); err != nil {
		return nil, err
	}
	conflictedWorktreeTar, missingWorktreePaths, err := buildConflictedWorktreeTar(repository, paths)
	if err != nil {
		return nil, err
	}
	state, err := readState(locations)
	if err != nil {
		return nil, err
	}
	stateTar, err := buildStateTar(state)
	if err != nil {
		return nil, err
	}
	named, err := namedObjects(runner, repository, unmerged, state)
	if err != nil {
		return nil, err
	}
	objects := gitSortedKeys(named)
	operation := &collectedOperation{
		metadata: core.SavedGitOperation{
			Unmerged:             unmerged,
			MissingWorktreePaths: missingWorktreePaths,
		},
		payloads: GitOperationPayloads{
			ConflictedWorktreeTar: conflictedWorktreeTar,
			StateTar:              stateTar,
		},
	}
	for _, object := range objects {
		switch kind := named[object]; kind {
		case "commit":
			operation.commits = append(operation.commits, object)
		case "tag":
			operation.tags = append(operation.tags, object)
		case "tree", "blob":
			operation.contents = append(operation.contents, [2]string{kind, object})
		default:
			return nil, fmt.Errorf("Git operation names object %s of unknown type %s", object, kind)
		}
	}
	if len(objects) > 0 {
		operation.metadata.Objects = objects
	}
	return operation, nil
}

func entryExists(path string) (bool, error) {
	_, err := os.Lstat(path)
	switch {
	case err == nil:
		return true, nil
	case errors.Is(err, fs.ErrNotExist):
		return false, nil
	default:
		return false, fmt.Errorf("inspect %s: %w", path, err)
	}
}

// gitPaths is where Git keeps each named Git-directory entry in this
// checkout, as (name, absolute path) pairs in the order of `names`.
func gitPaths(runner GitCommandRunner, repository string, names []string) ([][2]string, error) {
	arguments := []string{"rev-parse", "--path-format=absolute"}
	for _, name := range names {
		arguments = append(arguments, "--git-path", name)
	}
	output, err := gitSuccess(runner, repository, &GitCommand{Arguments: arguments}, "locate Git directory entries")
	if err != nil {
		return nil, err
	}
	lines := bytes.Split(bytes.TrimSuffix(output, []byte("\n")), []byte("\n"))
	if len(lines) != len(names) {
		return nil, fmt.Errorf("git rev-parse located %d of %d Git directory entries", len(lines), len(names))
	}
	located := make([][2]string, 0, len(names))
	for index, name := range names {
		path, err := core.FromGitBytes(lines[index])
		if err != nil {
			return nil, err
		}
		if !filepath.IsAbs(path) {
			return nil, fmt.Errorf("Git reported a relative location %s for %s", path, name)
		}
		located = append(located, [2]string{name, path})
	}
	return located, nil
}

// refuseIndexEntriesBelow fails when the index holds an entry below an
// unmerged path. A patch leaves out an excluded path together with
// everything below it, so such an entry would silently fall out of the
// staged patch. Git's merge backend never leaves one: it moves the file
// aside instead, as `path~HEAD`.
func refuseIndexEntriesBelow(runner GitCommandRunner, repository string, unmerged []string) error {
	unmergedPaths := make(map[string]struct{}, len(unmerged))
	for _, path := range unmerged {
		unmergedPaths[gitComponentKey(path)] = struct{}{}
	}
	listed, err := gitBytes(runner, repository, []string{"ls-files", "-z"}, nil, "list the clone's index")
	if err != nil {
		return err
	}
	for _, raw := range bytes.Split(listed, []byte{0}) {
		if len(raw) == 0 {
			continue
		}
		path, err := core.FromGitBytes(raw)
		if err != nil {
			return err
		}
		// Path::ancestors after the path itself, nearest first. Git names
		// index paths with `/`, so an ancestor is a `/`-joined prefix.
		components := archivePathComponents(path)
		texts := make([]string, len(components))
		for index, component := range components {
			texts[index] = component.text
		}
		for length := len(texts) - 1; length >= 1; length-- {
			ancestor := strings.Join(texts[:length], "/")
			if _, found := unmergedPaths[ancestor]; found {
				return fmt.Errorf("cannot checkpoint unmerged path '%s': the index also holds '%s' below it",
					ancestor, path)
			}
		}
	}
	return nil
}

// stateEntry is one Git-directory entry of an operation's state.
type stateEntry struct {
	// name is the name `git rev-parse --git-path` resolves, with `/` between
	// parts.
	name     string
	metadata fs.FileInfo
	// contents are the bytes of a file; hasContents is false for a
	// directory (Rust: Option<Vec<u8>>).
	contents    []byte
	hasContents bool
}

func (e *stateEntry) isNamedByTodoList() bool {
	return slices.Contains(todoLists[:], e.name)
}

func (e *stateEntry) holdsObjectNames() bool {
	return e.name != rerereCache && !strings.HasPrefix(e.name, "rr-cache/")
}

func readState(locations [][2]string) ([]stateEntry, error) {
	var entries []stateEntry
	for _, location := range locations {
		name, path := location[0], location[1]
		metadata, err := os.Lstat(path)
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return nil, fmt.Errorf("inspect %s: %w", path, err)
		}
		if slices.Contains(stateDirectories[:], name) {
			if !metadata.IsDir() {
				return nil, fmt.Errorf("Git operation state %s is not a directory", path)
			}
			if err := readStateDirectory(name, path, metadata, &entries); err != nil {
				return nil, err
			}
			continue
		}
		if !metadata.Mode().IsRegular() {
			return nil, fmt.Errorf("Git operation state %s is not a file", path)
		}
		contents, err := os.ReadFile(path)
		if err != nil {
			return nil, fmt.Errorf("read %s: %w", path, err)
		}
		entries = append(entries, stateEntry{name: name, metadata: metadata, contents: contents, hasContents: true})
	}
	return entries, nil
}

func readStateDirectory(name string, location string, metadata fs.FileInfo, entries *[]stateEntry) error {
	*entries = append(*entries, stateEntry{name: name, metadata: metadata})
	// os.ReadDir sorts by name, as Rust sorts the children by file name.
	children, err := os.ReadDir(location)
	if err != nil {
		return fmt.Errorf("list %s: %w", location, err)
	}
	for _, child := range children {
		path := gitJoin(location, child.Name())
		if !utf8.ValidString(child.Name()) {
			return fmt.Errorf("Git operation state %s is not UTF-8", path)
		}
		childName := name + "/" + child.Name()
		metadata, err := os.Lstat(path)
		if err != nil {
			return fmt.Errorf("inspect %s: %w", path, err)
		}
		if metadata.IsDir() {
			if err := readStateDirectory(childName, path, metadata, entries); err != nil {
				return err
			}
			continue
		}
		if !metadata.Mode().IsRegular() {
			return fmt.Errorf("unsupported Git operation state entry %s", path)
		}
		contents, err := os.ReadFile(path)
		if err != nil {
			return fmt.Errorf("read %s: %w", path, err)
		}
		*entries = append(*entries, stateEntry{name: childName, metadata: metadata, contents: contents, hasContents: true})
	}
	return nil
}

// deterministicTarTimestamp is the modification time the tar crate's
// `HeaderMode::Deterministic` writes for every entry.
const deterministicTarTimestamp = 1153704088

func buildStateTar(entries []stateEntry) ([]byte, error) {
	var output bytes.Buffer
	builder := tar.NewWriter(&output)
	for _, entry := range entries {
		// HeaderMode::Deterministic: a fixed time, no owner, and 0755 or 0644
		// after the owner's execute bit.
		header := &tar.Header{
			Name:    entry.name,
			Mode:    0o644,
			Size:    int64(len(entry.contents)),
			ModTime: time.Unix(deterministicTarTimestamp, 0),
			Format:  tar.FormatGNU,
		}
		switch {
		case entry.metadata.IsDir():
			header.Typeflag, header.Mode = tar.TypeDir, 0o755
		default:
			header.Typeflag = tar.TypeReg
			if entry.metadata.Mode().Perm()&0o100 != 0 {
				header.Mode = 0o755
			}
		}
		if err := builder.WriteHeader(header); err != nil {
			return nil, fmt.Errorf("archive Git operation state %s: %w", entry.name, err)
		}
		if _, err := builder.Write(entry.contents); err != nil {
			return nil, fmt.Errorf("archive Git operation state %s: %w", entry.name, err)
		}
	}
	if err := builder.Close(); err != nil {
		return nil, fmt.Errorf("finish Git operation state tar: %w", err)
	}
	return output.Bytes(), nil
}

// namedObjects is every object the unmerged entries and the state files
// name, keyed by id, with its type. A state file names an object by its full
// id, except for a todo list instruction, which may abbreviate the commit it
// names. A name that is no object here, such as a conflict hash in
// `MERGE_RR`, is skipped. A submodule conflict's stages name commits of the
// submodule, which this repository does not hold.
func namedObjects(
	runner GitCommandRunner,
	repository string,
	unmerged []core.SavedUnmergedEntry,
	state []stateEntry,
) (map[string]string, error) {
	// Each request, and whether only a commit may answer it.
	requests := map[string]bool{}
	request := func(name string, commitOnly bool) {
		if existing, found := requests[name]; found {
			commitOnly = existing && commitOnly
		}
		requests[name] = commitOnly
	}
	for _, entry := range unmerged {
		if entry.Mode != "160000" {
			request(entry.Object, false)
		}
	}
	for index := range state {
		entry := &state[index]
		if !entry.hasContents {
			continue
		}
		if entry.holdsObjectNames() {
			for _, name := range fullObjectIDs(entry.contents) {
				request(name, false)
			}
		}
		if entry.isNamedByTodoList() {
			for _, name := range todoCommits(entry.contents) {
				request(name, true)
			}
		}
	}
	if len(requests) == 0 {
		return map[string]string{}, nil
	}
	names := gitSortedKeys(requests)
	var input strings.Builder
	for _, name := range names {
		input.WriteString(name)
		input.WriteByte('\n')
	}
	output, err := gitBytes(runner, repository, []string{"cat-file", "--batch-check=%(objectname) %(objecttype)"},
		[]byte(input.String()), "resolve the objects a Git operation names")
	if err != nil {
		return nil, err
	}
	text, err := gitDecodeUTF8(output)
	if err != nil {
		return nil, fmt.Errorf("decode git cat-file --batch-check: %w", err)
	}
	answers := gitLines(text)
	if len(answers) != len(requests) {
		return nil, fmt.Errorf("git cat-file answered %d of %d object names", len(answers), len(requests))
	}
	named := map[string]string{}
	for index, name := range names {
		answer := answers[index]
		object, kind, found := strings.Cut(answer, " ")
		if !found {
			return nil, fmt.Errorf("malformed git cat-file answer %s", gitRustDebugString(answer))
		}
		if kind == "missing" || kind == "ambiguous" || (requests[name] && kind != "commit") {
			continue
		}
		if !core.IsObjectID(object) {
			return nil, fmt.Errorf("git cat-file answered %s", gitRustDebugString(answer))
		}
		named[object] = kind
	}
	return named, nil
}

// fullObjectIDs is every full SHA-1 or SHA-256 id in `contents`, as a word
// on its own.
func fullObjectIDs(contents []byte) []string {
	isAlphanumeric := func(b byte) bool {
		return b >= '0' && b <= '9' || b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z'
	}
	var ids []string
	for _, word := range bytes.FieldsFunc(contents, func(r rune) bool { return r >= utf8.RuneSelf || !isAlphanumeric(byte(r)) }) {
		if len(word) != 40 && len(word) != 64 {
			continue
		}
		if !slices.ContainsFunc(word, func(b byte) bool { return !(b >= '0' && b <= '9' || b >= 'a' && b <= 'f') }) {
			ids = append(ids, string(word))
		}
	}
	return ids
}

// todoCommits are the commits a todo list's instructions name, as written:
// `pick <commit>` and its kin, `fixup -C <commit>`,
// `merge -C <commit> <label>` and `reset <commit>`. Labels and comments are
// not commits.
func todoCommits(contents []byte) []string {
	var commits []string
	for _, line := range gitLines(gitFromUTF8Lossy(contents)) {
		// split_ascii_whitespace: space, tab, line feed, form feed and
		// carriage return.
		words := strings.FieldsFunc(line, func(r rune) bool {
			return r == ' ' || r == '\t' || r == '\n' || r == '\f' || r == '\r'
		})
		if len(words) == 0 {
			continue
		}
		switch words[0] {
		case "pick", "p", "reword", "r", "edit", "e", "squash", "s", "fixup", "f", "drop", "d",
			"revert", "merge", "m", "reset", "t":
		default:
			continue
		}
		argument := 1
		if argument < len(words) && (words[argument] == "-C" || words[argument] == "-c") {
			argument++
		}
		if argument >= len(words) {
			continue
		}
		word := words[argument]
		if len(word) < 4 || len(word) > 64 {
			continue
		}
		if slices.ContainsFunc([]byte(word), func(b byte) bool {
			return !(b >= '0' && b <= '9' || b >= 'a' && b <= 'f' || b >= 'A' && b <= 'F')
		}) {
			continue
		}
		commits = append(commits, strings.ToLower(word))
	}
	return commits
}

// verifyObjects fails unless every object the operation names is in the
// restored clone. The bundle was imported just before, so a missing object
// means the archive cannot give the operation back.
func verifyObjects(runner GitCommandRunner, repository string, operation *core.SavedGitOperation) error {
	if len(operation.Objects) == 0 {
		return nil
	}
	var input strings.Builder
	for _, object := range operation.Objects {
		input.WriteString(object)
		input.WriteByte('\n')
	}
	output, err := gitBytes(runner, repository, []string{"cat-file", "--batch-check=%(objectname)"},
		[]byte(input.String()), "verify the Git operation's objects")
	if err != nil {
		return err
	}
	text, err := gitDecodeUTF8(output)
	if err != nil {
		return fmt.Errorf("decode git cat-file --batch-check: %w", err)
	}
	answers := gitLines(text)
	if len(answers) != len(operation.Objects) {
		return fmt.Errorf("git cat-file answered %d of %d Git operation objects", len(answers), len(operation.Objects))
	}
	for index, object := range operation.Objects {
		if !archiveEqualFoldASCII(answers[index], object) {
			return fmt.Errorf("restore Git operation: object %s is missing from the restored clone", object)
		}
	}
	return nil
}

// operationRestore writes the operation back into a clone whose commits,
// index and working tree are otherwise restored: the unmerged index entries,
// the files of the unmerged paths, and the state files. (Rust:
// operation::restore.)
func operationRestore(
	runner GitCommandRunner,
	repository string,
	operation *core.SavedGitOperation,
	payloads *GitOperationPayloads,
) error {
	paths := uniquePaths(operation.Unmerged)
	if len(paths) > 0 {
		// A mode-0 record removes every stage of its path, which is how Git
		// asks for a path's stage-0 entry to give way to its conflict stages.
		var records []byte
		started := map[string]struct{}{}
		for _, entry := range operation.Unmerged {
			if key := gitComponentKey(entry.Path); !mapsContains(started, key) {
				started[key] = struct{}{}
				records = fmt.Appendf(records, "0 %s\t", strings.Repeat("0", len(entry.Object)))
				records = append(records, entry.Path...)
				records = append(records, 0)
			}
			records = fmt.Appendf(records, "%s %s %d\t", entry.Mode, entry.Object, entry.Stage)
			records = append(records, entry.Path...)
			records = append(records, 0)
		}
		if _, err := gitBytes(runner, repository, []string{"update-index", "-z", "--index-info"}, records,
			"restore the unmerged index entries"); err != nil {
			return err
		}
	}

	unmerged := make(map[string]struct{}, len(paths))
	for _, path := range paths {
		unmerged[gitComponentKey(path)] = struct{}{}
	}
	archive := tar.NewReader(bytes.NewReader(payloads.ConflictedWorktreeTar))
	for {
		header, err := archive.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("read conflicted-file tar entry: %w", err)
		}
		if !mapsContains(unmerged, gitComponentKey(header.Name)) {
			return fmt.Errorf("conflicted-file tar holds '%s', which is not unmerged", header.Name)
		}
	}
	if err := restoreWorktreeTar(repository, payloads.ConflictedWorktreeTar, worktreeTarConflicted); err != nil {
		return err
	}
	for _, path := range operation.MissingWorktreePaths {
		if !mapsContains(unmerged, gitComponentKey(path)) {
			return fmt.Errorf("missing working-tree path '%s' is not unmerged", path)
		}
		if err := ensureNoSymlinkAncestors(repository, path); err != nil {
			return err
		}
		if err := removeWorktreeFile(gitJoin(repository, path)); err != nil {
			return err
		}
	}
	return restoreState(runner, repository, payloads.StateTar)
}

func mapsContains(set map[string]struct{}, key string) bool {
	_, found := set[key]
	return found
}

func restoreState(runner GitCommandRunner, repository string, data []byte) error {
	if err := validateOperationStateTar(data); err != nil {
		return err
	}
	type archivedState struct {
		path      string
		directory bool
		mode      int64
		contents  []byte
	}
	var entries []archivedState
	archive := tar.NewReader(bytes.NewReader(data))
	for {
		header, err := archive.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("read Git operation state tar entry: %w", err)
		}
		contents, err := io.ReadAll(archive)
		if err != nil {
			return fmt.Errorf("read Git operation state %s: %w", header.Name, err)
		}
		entries = append(entries, archivedState{
			path:      header.Name,
			directory: header.Typeflag == tar.TypeDir,
			mode:      header.Mode,
			contents:  contents,
		})
	}
	var names []string
	for _, entry := range entries {
		if name, ok := topLevelStateName(entry.path); ok {
			names = append(names, name)
		}
	}
	slices.Sort(names)
	located, err := gitPaths(runner, repository, slices.Compact(names))
	if err != nil {
		return err
	}
	locations := make(map[string]string, len(located))
	for _, location := range located {
		locations[location[0]] = location[1]
	}
	for _, entry := range entries {
		name, ok := topLevelStateName(entry.path)
		if !ok {
			return errors.New("Git operation state entry has no name")
		}
		destination := locations[name]
		for _, component := range archivePathComponents(entry.path)[1:] {
			destination = gitJoin(destination, component.text)
		}
		if entry.directory {
			if err := os.MkdirAll(destination, 0o777); err != nil {
				return fmt.Errorf("create %s: %w", destination, err)
			}
			continue
		}
		parent := filepath.Dir(destination)
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return fmt.Errorf("create %s: %w", parent, err)
		}
		file, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o666)
		if errors.Is(err, fs.ErrExist) && name == rerereCache {
			// A recorded resolution the clone already has is as good as the
			// archived one, and the cache is shared by the repository's
			// worktrees.
			continue
		}
		if err != nil {
			return fmt.Errorf("write Git operation state %s: %w", destination, err)
		}
		_, err = file.Write(entry.contents)
		if err == nil {
			err = file.Sync()
		}
		if closed := file.Close(); err == nil {
			err = closed
		}
		if err != nil {
			return fmt.Errorf("write Git operation state %s: %w", destination, err)
		}
		if runtime.GOOS != "windows" {
			if err := os.Chmod(destination, fs.FileMode(entry.mode&0o777)); err != nil {
				return err
			}
		}
	}
	return nil
}

// topLevelStateName is the state entry an archived path belongs to: its
// first component, when that is one Git keeps.
func topLevelStateName(path string) (string, bool) {
	components := archivePathComponents(path)
	if len(components) == 0 || components[0].kind != archiveComponentNormal {
		return "", false
	}
	for _, name := range slices.Concat(stateFiles[:], stateDirectories[:]) {
		if components[0].text == name {
			return name, true
		}
	}
	return "", false
}

func validateOperationStateTar(data []byte) error {
	return validateOperationStateTarReader(bytes.NewReader(data))
}

// validateOperationStateTarReader checks that every entry of an operation
// state tar is a Git state file or lies inside a Git state directory, so a
// restore writes only those.
func validateOperationStateTarReader(reader io.Reader) error {
	archive := tar.NewReader(reader)
	for {
		header, err := archive.Next()
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return fmt.Errorf("read Git operation state tar entry: %w", err)
		}
		path := header.Name
		if err := validateArchiveRelativePath(path); err != nil {
			return err
		}
		name, ok := topLevelStateName(path)
		if !ok {
			return fmt.Errorf("Git operation state tar holds '%s', which is not Git operation state", path)
		}
		if slices.Contains(stateDirectories[:], name) {
			if !gitTarIsFile(header) && header.Typeflag != tar.TypeDir {
				return fmt.Errorf("Git operation state '%s' is neither a file nor a directory", path)
			}
		} else if !gitTarIsFile(header) || len(archivePathComponents(path)) != 1 {
			return fmt.Errorf("Git operation state '%s' is not a file", path)
		}
	}
}
