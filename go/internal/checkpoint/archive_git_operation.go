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
	"bytes"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"slices"
	"strings"

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
	panic("unported: mj-checkpoint/src/archive/git/operation.rs CollectedOperation::carry_objects")
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
	panic("unported: mj-checkpoint/src/archive/git/operation.rs collect")
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
	panic("unported: mj-checkpoint/src/archive/git/operation.rs git_paths")
}

// refuseIndexEntriesBelow fails when the index holds an entry below an
// unmerged path. A patch leaves out an excluded path together with
// everything below it, so such an entry would silently fall out of the
// staged patch. Git's merge backend never leaves one: it moves the file
// aside instead, as `path~HEAD`.
func refuseIndexEntriesBelow(runner GitCommandRunner, repository string, unmerged []string) error {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs refuse_index_entries_below")
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
	panic("unported: mj-checkpoint/src/archive/git/operation.rs read_state")
}

func readStateDirectory(name string, location string, metadata fs.FileInfo, entries *[]stateEntry) error {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs read_state_directory")
}

func buildStateTar(entries []stateEntry) ([]byte, error) {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs build_state_tar")
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
	panic("unported: mj-checkpoint/src/archive/git/operation.rs named_objects")
}

// fullObjectIDs is every full SHA-1 or SHA-256 id in `contents`, as a word
// on its own.
func fullObjectIDs(contents []byte) []string {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs full_object_ids")
}

// todoCommits are the commits a todo list's instructions name, as written:
// `pick <commit>` and its kin, `fixup -C <commit>`,
// `merge -C <commit> <label>` and `reset <commit>`. Labels and comments are
// not commits.
func todoCommits(contents []byte) []string {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs todo_commits")
}

// verifyObjects fails unless every object the operation names is in the
// restored clone. The bundle was imported just before, so a missing object
// means the archive cannot give the operation back.
func verifyObjects(runner GitCommandRunner, repository string, operation *core.SavedGitOperation) error {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs verify_objects")
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
	panic("unported: mj-checkpoint/src/archive/git/operation.rs restore")
}

func restoreState(runner GitCommandRunner, repository string, data []byte) error {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs restore_state")
}

// topLevelStateName is the state entry an archived path belongs to: its
// first component, when that is one Git keeps.
func topLevelStateName(path string) (string, bool) {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs top_level_state_name")
}

func validateOperationStateTar(data []byte) error {
	return validateOperationStateTarReader(bytes.NewReader(data))
}

// validateOperationStateTarReader checks that every entry of an operation
// state tar is a Git state file or lies inside a Git state directory, so a
// restore writes only those.
func validateOperationStateTarReader(reader io.Reader) error {
	panic("unported: mj-checkpoint/src/archive/git/operation.rs validate_operation_state_tar_reader")
}
