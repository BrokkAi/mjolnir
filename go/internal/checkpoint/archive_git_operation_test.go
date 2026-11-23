// Port of mj-checkpoint/src/archive/git/operation/tests.rs.
//
// A clone stopped in the middle of a Git operation goes through a real
// legacy checkpoint archive and a resume-style restore, which drops the
// operation's state with a notice and keeps its conflicted working files as
// ordinary unstaged edits.

package checkpoint

import (
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// operationStep runs Git with a fixed clock and an editor that accepts every
// message, so the same step taken in two copies of a repository writes the
// same commits.
func operationStep(t testing.TB, repository string, arguments ...string) (int, string) {
	t.Helper()
	command := exec.Command("git", arguments...)
	command.Dir = repository
	command.Env = append(os.Environ(),
		"GIT_AUTHOR_DATE=@1700000000 +0000",
		"GIT_COMMITTER_DATE=@1700000000 +0000",
		"GIT_EDITOR=true",
	)
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		t.Fatalf("git %q in %s: %v", arguments, repository, err)
	}
	return output.Status.ExitCode(), gitFromUTF8Lossy(output.Stdout) + gitFromUTF8Lossy(output.Stderr)
}

func operationStepOK(t testing.TB, repository string, arguments ...string) {
	t.Helper()
	if status, output := operationStep(t, repository, arguments...); status != 0 {
		t.Fatalf("git %q in %s: %s", arguments, repository, output)
	}
}

// operationStepStops starts an operation that has to stop for the person to
// resolve conflicts.
func operationStepStops(t testing.TB, repository string, arguments ...string) {
	t.Helper()
	if status, output := operationStep(t, repository, arguments...); status == 0 {
		t.Fatalf("git %q did not stop: %s", arguments, output)
	}
}

const operationABase = "1\n2\n3\n4\n5\n6\n7\n8\n9\n"

// operationChange is one path to write, or to delete when contents is nil.
type operationChange struct {
	path     string
	contents *string
}

func someText(text string) *string { return &text }

// operationMainChanges and operationSideChanges are what each history below
// writes: two separate hunks of `a.txt`, all of `b.txt`, `m.txt` that one
// side deletes, and the symbolic link `link`.
func operationMainChanges() []operationChange {
	return []operationChange{
		{"a.txt", someText("1\nmain 2\n3\n4\n5\n6\n7\nmain 8\n9\n")},
		{"b.txt", someText("main b\n")},
		{"m.txt", someText("main m\n")},
		{"link", someText("b.txt")},
	}
}

func operationSideChanges() []operationChange {
	return []operationChange{
		{"a.txt", someText("1\nside 2\n3\n4\n5\n6\n7\nside 8\n9\n")},
		{"b.txt", someText("side b\n")},
		{"m.txt", nil},
		{"link", someText("c.txt")},
	}
}

type operationLab struct {
	directory string
	origin    string
	source    string
	base      string
}

func newOperationLab(t *testing.T) *operationLab {
	t.Helper()
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	operationApply(t, origin, []operationChange{
		{"a.txt", someText(operationABase)},
		{"b.txt", someText("b\n")},
		{"c.txt", someText("c\n")},
		{"d.txt", someText("d\n")},
		{"e.txt", someText("e\n")},
		{"m.txt", someText("m\n")},
		{"link", someText("a.txt")},
	})
	base := operationCommit(t, origin, "base")
	source := cloneRepository(t, directory, origin, "source")
	return &operationLab{directory: directory, origin: origin, source: source, base: base}
}

// operationApply writes each path, deleting it for nil. `link` is a symbolic
// link to the named target where the platform has them.
func operationApply(t testing.TB, repository string, changes []operationChange) {
	t.Helper()
	for _, change := range changes {
		destination := filepath.Join(repository, change.path)
		if change.path == "link" {
			if runtime.GOOS != "windows" {
				_ = os.Remove(destination)
				if change.contents != nil {
					if err := os.Symlink(*change.contents, destination); err != nil {
						t.Fatal(err)
					}
				}
			}
			continue
		}
		if change.contents != nil {
			if err := os.WriteFile(destination, []byte(*change.contents), 0o666); err != nil {
				t.Fatal(err)
			}
		} else if err := os.Remove(destination); err != nil {
			t.Fatal(err)
		}
	}
}

func operationCommit(t testing.TB, repository string, message string) string {
	t.Helper()
	operationStepOK(t, repository, "add", "-A")
	operationStepOK(t, repository, "commit", "-qm", message)
	return gitLine(repository, "rev-parse", "HEAD")
}

func operationCommitChanges(t testing.TB, repository string, message string, changes []operationChange) string {
	t.Helper()
	operationApply(t, repository, changes)
	return operationCommit(t, repository, message)
}

// operationDiverged is `main` and `side` both changing every file, so any
// operation between them stops on conflicts; `side` has a second commit
// that applies cleanly.
type operationDiverged struct {
	main      string
	sideFirst string
	sideTip   string
}

func operationDiverge(t testing.TB, lab *operationLab) operationDiverged {
	t.Helper()
	main := operationCommitChanges(t, lab.source, "main work", operationMainChanges())
	operationStepOK(t, lab.source, "switch", "-q", "-c", "side", lab.base)
	sideFirst := operationCommitChanges(t, lab.source, "side work", operationSideChanges())
	sideTip := operationCommitChanges(t, lab.source, "side extra", []operationChange{{"s.txt", someText("side s\n")}})
	return operationDiverged{main: main, sideFirst: sideFirst, sideTip: sideTip}
}

// operationDirtyBeforeStarting dirties a tracked file the operation does not
// touch, for `--autostash`.
func operationDirtyBeforeStarting(t testing.TB, repository string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(repository, "e.txt"), []byte("autostashed e\n"), 0o666); err != nil {
		t.Fatal(err)
	}
}

// operationPartlyResolve resolves `b.txt` completely and one of the two
// conflicts in `a.txt`, takes `m.txt` out of the working tree without
// resolving it, and adds unrelated staged, unstaged and untracked work.
func operationPartlyResolve(t testing.TB, repository string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(repository, "b.txt"), []byte("resolved b\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	operationStepOK(t, repository, "add", "b.txt")
	data, err := os.ReadFile(filepath.Join(repository, "a.txt"))
	if err != nil {
		t.Fatal(err)
	}
	conflicted := string(data)
	start := strings.Index(conflicted, "<<<<<<<")
	if start < 0 {
		t.Fatal("a.txt has a conflict")
	}
	end := start + strings.Index(conflicted[start:], ">>>>>>>")
	end = end + strings.IndexByte(conflicted[end:], '\n') + 1
	resolved := conflicted[:start] + "resolved 2\n" + conflicted[end:]
	if !strings.Contains(resolved, "<<<<<<<") {
		t.Fatal("a.txt keeps its second conflict")
	}
	if err := os.WriteFile(filepath.Join(repository, "a.txt"), []byte(resolved), 0o666); err != nil {
		t.Fatal(err)
	}
	_ = os.Remove(filepath.Join(repository, "m.txt"))
	operationUnrelatedWork(t, repository)
	unmerged := gitFromUTF8Lossy(git(repository, "ls-files", "--unmerged"))
	if !strings.Contains(unmerged, "\ta.txt") || !strings.Contains(unmerged, "\tm.txt") {
		t.Fatal(unmerged)
	}
}

func operationUnrelatedWork(t testing.TB, repository string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(repository, "c.txt"), []byte("staged c\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	operationStepOK(t, repository, "add", "c.txt")
	if err := os.WriteFile(filepath.Join(repository, "d.txt"), []byte("unstaged d\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(repository, "notes"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(repository, "notes/untracked.txt"), []byte("untracked\n"), 0o666); err != nil {
		t.Fatal(err)
	}
}

// operationWorktreeEntry is one working-tree path: missing, a file, or a
// symbolic link.
type operationWorktreeEntry struct {
	kind       string
	bytes      []byte
	executable bool
	target     string
}

// operationObserved is everything about a checkout that the operation
// depends on or that a person can see of it.
type operationObserved struct {
	head         string
	status       string
	index        string
	refs         string
	worktree     map[string]operationWorktreeEntry
	gitDirectory map[string][]byte
}

func operationObserve(t testing.TB, repository string) operationObserved {
	t.Helper()
	text := func(arguments ...string) string { return gitFromUTF8Lossy(git(repository, arguments...)) }
	listed := git(repository, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
	worktree := map[string]operationWorktreeEntry{}
	for _, raw := range strings.Split(string(listed), "\x00") {
		if raw == "" {
			continue
		}
		file := filepath.Join(repository, raw)
		metadata, err := os.Lstat(file)
		var entry operationWorktreeEntry
		switch {
		case err != nil:
			entry = operationWorktreeEntry{kind: "missing"}
		case metadata.Mode()&fs.ModeSymlink != 0:
			target, err := os.Readlink(file)
			if err != nil {
				t.Fatal(err)
			}
			entry = operationWorktreeEntry{kind: "symlink", target: target}
		default:
			contents, err := os.ReadFile(file)
			if err != nil {
				t.Fatal(err)
			}
			entry = operationWorktreeEntry{
				kind:       "file",
				bytes:      contents,
				executable: runtime.GOOS != "windows" && metadata.Mode().Perm()&0o111 != 0,
			}
		}
		worktree[raw] = entry
	}
	gitDir := gitLine(repository, "rev-parse", "--absolute-git-dir")
	gitDirectory := map[string][]byte{}
	operationCollectGitDirectory(t, gitDir, gitDir, gitDirectory)
	return operationObserved{
		head:         text("rev-parse", "--symbolic-full-name", "HEAD") + text("rev-parse", "HEAD"),
		status:       text("status", "--porcelain=v2", "-z", "--branch"),
		index:        text("ls-files", "--stage", "-z"),
		refs:         text("for-each-ref", "--format=%(refname) %(objectname)"),
		worktree:     worktree,
		gitDirectory: gitDirectory,
	}
}

// operationCollectGitDirectory reads every file in the Git directory except
// the object store, refs, logs, the index (compared through `ls-files`), and
// files a clone or a commit writes for itself. Whatever else is there is
// operation state, so a state file the capture missed shows up here as a
// difference.
func operationCollectGitDirectory(t testing.TB, root string, directory string, found map[string][]byte) {
	t.Helper()
	notOperationState := []string{
		"objects",
		"refs",
		"logs",
		"hooks",
		"info",
		"branches",
		"index",
		"config",
		"description",
		"packed-refs",
		"HEAD",
		"FETCH_HEAD",
		"COMMIT_EDITMSG",
	}
	entries, err := os.ReadDir(directory)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		path := filepath.Join(directory, entry.Name())
		relative, err := filepath.Rel(root, path)
		if err != nil {
			t.Fatal(err)
		}
		relative = strings.ReplaceAll(relative, "\\", "/")
		if directory == root && slices.Contains(notOperationState, relative) {
			continue
		}
		if entry.IsDir() {
			found[relative+"/"] = nil
			operationCollectGitDirectory(t, root, path, found)
			continue
		}
		contents, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		found[relative] = contents
	}
}

func operationCapture(t testing.TB, source string, base string) RepositorySnapshot {
	t.Helper()
	snapshot, err := CollectGitSnapshot(git231{}, source, &GitCollectionSpec{
		ID:                  "repo",
		RelativeDestination: "repo",
		History:             GitHistoryModeCloneFrom(base),
	})
	if err != nil {
		t.Fatal(err)
	}
	if refs := gitLine(source, "for-each-ref", "refs/mj"); refs != "" {
		t.Fatalf("capture left temporary refs behind: %s", refs)
	}
	return snapshot
}

// operationRestoreArchive restores the archive the way resume does: into a
// fresh clone of origin, from the verified archive file, returning the
// checkout path and the resume notices.
func operationRestoreArchive(t testing.TB, lab *operationLab, archive string, name string) (string, []string) {
	t.Helper()
	workspace := filepath.Join(lab.directory, name)
	if err := os.MkdirAll(workspace, 0o777); err != nil {
		t.Fatal(err)
	}
	destination := cloneRepository(t, workspace, lab.origin, "repo")
	notices, err := RestoreSingleRepositoryIntoCheckout(archive, destination, git231{})
	if err != nil {
		t.Fatal(err)
	}
	return destination, notices
}

// operationRoundTripDroppedOperation checkpoints the source through a real
// legacy archive, restores it, and checks that the in-progress Git
// operation state is dropped with a notice while conflicted working files
// survive as ordinary unstaged edits.
func operationRoundTripDroppedOperation(t testing.TB, lab *operationLab, expectedKind string, expectedConflictedPaths []string) (string, []string) {
	t.Helper()
	snapshot := operationCapture(t, lab.source, lab.base)
	archive := filepath.Join(lab.directory, "checkpoint.hel.zip")
	archiveInput := input()
	archiveInput.Bundle.PrimaryRepository = "repo"
	archiveInput.Repositories = []RepositorySnapshot{snapshot}
	if _, err := WriteArchiveAtomic(archive, &archiveInput); err != nil {
		t.Fatal(err)
	}

	restored, notices := operationRestoreArchive(t, lab, archive, "restored")
	before := operationObserve(t, lab.source)
	after := operationObserve(t, restored)

	// Working-tree files (including conflicted files, missing worktree
	// paths, staged/unstaged edits, and untracked files) survive as ordinary
	// files. A path that was both unmerged and missing from the working tree
	// may no longer appear in `ls-files` once unmerged index stages are
	// dropped.
	expectedWorktree := maps.Clone(before.worktree)
	maps.DeleteFunc(expectedWorktree, func(path string, entry operationWorktreeEntry) bool {
		_, listed := after.worktree[path]
		return entry.kind == "missing" && !listed
	})
	if !reflect.DeepEqual(after.worktree, expectedWorktree) {
		t.Fatalf("restored worktree %+v, want %+v", after.worktree, expectedWorktree)
	}
	// No active Git operation state or unmerged index stages remain.
	if len(after.gitDirectory) != 0 {
		t.Fatalf("expected no active Git operation state, found: %q", gitSortedKeys(after.gitDirectory))
	}
	if unmerged := gitLine(restored, "ls-files", "--unmerged"); unmerged != "" {
		t.Fatalf("expected no unmerged index entries after dropping legacy operation state: %q", unmerged)
	}
	if len(notices) != 1 {
		t.Fatalf("expected one resume notice, got: %q", notices)
	}
	if !strings.Contains(notices[0], expectedKind) {
		t.Fatalf("expected notice to name '%s': %s", expectedKind, notices[0])
	}
	for _, path := range expectedConflictedPaths {
		if !strings.Contains(notices[0], path) {
			t.Fatalf("expected notice to name preserved conflicted path '%s': %s", path, notices[0])
		}
	}
	return restored, notices
}

// Hard-won: #1319: a clone mid-merge could not be checkpointed, so suspend failed forever
// Hard-won: 5a3457a30: Git 2.25 echoed --path-format=absolute, so every clone checkpoint failed to locate its operation state.
func TestACloneStoppedInAConflictedMergeDropsOperationAndPreservesConflictedFiles(t *testing.T) {
	lab := newOperationLab(t)
	diverged := operationDiverge(t, lab)
	operationStepOK(t, lab.source, "switch", "-q", "main")
	if err := os.Mkdir(filepath.Join(lab.source, ".git/rr-cache"), 0o777); err != nil {
		t.Fatal(err)
	}
	operationDirtyBeforeStarting(t, lab.source)
	operationStepStops(t, lab.source, "merge", "--autostash", "side")
	operationPartlyResolve(t, lab.source)

	restored, _ := operationRoundTripDroppedOperation(t, lab, "merge", []string{"a.txt", "link", "m.txt"})
	if head := gitLine(restored, "rev-parse", "HEAD"); head != diverged.main {
		t.Fatalf("HEAD %s, want %s", head, diverged.main)
	}
}

func TestACloneStoppedInACherryPickSequenceDropsOperationAndPreservesConflictedFiles(t *testing.T) {
	lab := newOperationLab(t)
	diverged := operationDiverge(t, lab)
	operationStepOK(t, lab.source, "switch", "-q", "main")
	operationStepOK(t, lab.source, "branch", "-q", "-D", "side")
	operationStepStops(t, lab.source, "cherry-pick", diverged.sideFirst, diverged.sideTip)
	operationPartlyResolve(t, lab.source)

	restored, _ := operationRoundTripDroppedOperation(t, lab, "cherry-pick", []string{"a.txt", "link", "m.txt"})
	if head := gitLine(restored, "rev-parse", "HEAD"); head != diverged.main {
		t.Fatalf("HEAD %s, want %s", head, diverged.main)
	}
}

func TestACloneStoppedInAConflictedRevertDropsOperationAndPreservesConflictedFiles(t *testing.T) {
	lab := newOperationLab(t)
	reverted := operationCommitChanges(t, lab.source, "main work", operationMainChanges())
	later := operationCommitChanges(t, lab.source, "later work", operationSideChanges())
	operationStepStops(t, lab.source, "revert", reverted)
	operationPartlyResolve(t, lab.source)

	restored, _ := operationRoundTripDroppedOperation(t, lab, "revert", []string{"a.txt", "link", "m.txt"})
	if head := gitLine(restored, "rev-parse", "HEAD"); head != later {
		t.Fatalf("HEAD %s, want %s", head, later)
	}
}

func TestACloneStoppedInARebaseDropsOperationAndPreservesConflictedFiles(t *testing.T) {
	for _, backend := range []string{"--merge", "--apply"} {
		lab := newOperationLab(t)
		operationDiverge(t, lab)
		operationDirtyBeforeStarting(t, lab.source)
		operationStepStops(t, lab.source, "rebase", backend, "--autostash", "main")
		operationPartlyResolve(t, lab.source)

		operationRoundTripDroppedOperation(t, lab, "rebase", []string{"a.txt", "link", "m.txt"})
	}
}

func TestACloneInTheMiddleOfABisectDropsOperationWithNotice(t *testing.T) {
	lab := newOperationLab(t)
	for number := 1; number <= 6; number++ {
		operationCommitChanges(t, lab.source, fmt.Sprintf("history %d", number),
			[]operationChange{{"h.txt", someText(fmt.Sprintf("%d\n", number))}})
	}
	operationStepOK(t, lab.source, "bisect", "start")
	operationStepOK(t, lab.source, "bisect", "bad")
	operationStepOK(t, lab.source, "bisect", "good", lab.base)
	operationStepOK(t, lab.source, "bisect", "bad")
	operationUnrelatedWork(t, lab.source)

	restored, _ := operationRoundTripDroppedOperation(t, lab, "bisect", nil)
	if refs := gitLine(restored, "for-each-ref", "refs/bisect"); refs != "" {
		t.Fatalf("refs/bisect = %q", refs)
	}
}

func TestACaptureThatFailsMidOperationLeavesNoTemporaryRefs(t *testing.T) {
	lab := newOperationLab(t)
	operationDiverge(t, lab)
	operationStepOK(t, lab.source, "switch", "-q", "main")
	operationStepStops(t, lab.source, "merge", "side")
	operationPartlyResolve(t, lab.source)

	_, err := CollectGitSnapshotWithProgress(SystemGit{}, lab.source, &GitCollectionSpec{
		ID:                  "repo",
		RelativeDestination: "repo",
		History:             GitHistoryModeCloneFrom(lab.base),
	}, UntrackedFilesInclude, func(GitSnapshotProgress) error {
		return errors.New("stopped by the caller")
	})
	if err == nil || !strings.Contains(err.Error(), "stopped by the caller") {
		t.Fatalf("capture error %v", err)
	}
	if refs := gitLine(lab.source, "for-each-ref", "refs/mj"); refs != "" {
		t.Fatalf("temporary refs left behind: %s", refs)
	}
}
