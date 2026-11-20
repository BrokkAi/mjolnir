// Port of mj-checkpoint/src/archive/git/operation/tests.rs.
//
// A clone stopped in the middle of a Git operation goes through a real
// checkpoint archive and a resume-style restore, and both copies must then
// behave the same way under Git's own next step.

package checkpoint

import (
	"errors"
	"fmt"
	"io/fs"
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

// operationFinishResolving settles every path operationPartlyResolve left
// unmerged.
var operationFinishResolvingSteps = [][]string{{"add", "a.txt", "link"}, {"rm", "-q", "m.txt"}}

func operationFinishResolving(t testing.TB, repository string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(repository, "a.txt"),
		[]byte("1\nresolved 2\n3\n4\n5\n6\n7\nresolved 8\n9\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	for _, arguments := range operationFinishResolvingSteps {
		arguments = slices.DeleteFunc(slices.Clone(arguments), func(argument string) bool {
			return runtime.GOOS == "windows" && argument == "link"
		})
		operationStepOK(t, repository, arguments...)
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

// operationAssertSame fails when two observations differ.
func operationAssertSame(t testing.TB, got operationObserved, want operationObserved) {
	t.Helper()
	if reflect.DeepEqual(got, want) {
		return
	}
	var differences []string
	compare := func(name string, left, right string) {
		if left != right {
			differences = append(differences, fmt.Sprintf("%s: got %q, want %q", name, left, right))
		}
	}
	compare("head", got.head, want.head)
	compare("status", got.status, want.status)
	compare("index", got.index, want.index)
	compare("refs", got.refs, want.refs)
	for _, path := range gitSortedKeys(got.worktree) {
		if !reflect.DeepEqual(got.worktree[path], want.worktree[path]) {
			differences = append(differences, fmt.Sprintf("worktree %s: got %+v, want %+v", path, got.worktree[path], want.worktree[path]))
		}
	}
	for _, path := range gitSortedKeys(want.worktree) {
		if _, found := got.worktree[path]; !found {
			differences = append(differences, fmt.Sprintf("worktree %s: missing, want %+v", path, want.worktree[path]))
		}
	}
	for _, path := range gitSortedKeys(got.gitDirectory) {
		wantContents, found := want.gitDirectory[path]
		if !found || string(got.gitDirectory[path]) != string(wantContents) {
			differences = append(differences, fmt.Sprintf("git directory %s: got %q, want %q (present %v)",
				path, got.gitDirectory[path], wantContents, found))
		}
	}
	for _, path := range gitSortedKeys(want.gitDirectory) {
		if _, found := got.gitDirectory[path]; !found {
			differences = append(differences, fmt.Sprintf("git directory %s: missing, want %q", path, want.gitDirectory[path]))
		}
	}
	t.Fatalf("observations differ:\n%s", strings.Join(differences, "\n"))
}

// operationCopyTree makes an exact copy of a checkout, Git directory
// included.
func operationCopyTree(t testing.TB, from string, to string) {
	t.Helper()
	if err := os.MkdirAll(to, 0o777); err != nil {
		t.Fatal(err)
	}
	entries, err := os.ReadDir(from)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		source := filepath.Join(from, entry.Name())
		target := filepath.Join(to, entry.Name())
		switch {
		case entry.IsDir():
			operationCopyTree(t, source, target)
		case entry.Type()&fs.ModeSymlink != 0:
			if runtime.GOOS != "windows" {
				link, err := os.Readlink(source)
				if err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(link, target); err != nil {
					t.Fatal(err)
				}
			}
		default:
			info, err := entry.Info()
			if err != nil {
				t.Fatal(err)
			}
			contents, err := os.ReadFile(source)
			if err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(target, contents, info.Mode().Perm()); err != nil {
				t.Fatal(err)
			}
			if err := os.Chmod(target, info.Mode().Perm()); err != nil {
				t.Fatal(err)
			}
		}
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

// operationRestoreArchive restores the archive the way resume does: into a fresh
// clone of origin, from the verified archive file.
func operationRestoreArchive(t testing.TB, lab *operationLab, archive string, name string) string {
	t.Helper()
	workspace := filepath.Join(lab.directory, name)
	if err := os.MkdirAll(workspace, 0o777); err != nil {
		t.Fatal(err)
	}
	destination := cloneRepository(t, workspace, lab.origin, "repo")
	if err := RestoreRepositories(archive, workspace, git231{}); err != nil {
		t.Fatal(err)
	}
	return destination
}

type operationRoundTrip struct {
	finishedSource    string
	finishedRestored  string
	abandonedCopy     string
	abandonedRestored string
}

// operationRoundTripThrough checkpoints the source through a real archive,
// restores it twice, and checks that each restored copy is the source: first
// as it stands, then after Git's own step that finishes the operation (taken
// in the source and one copy) and after the step that abandons it (taken in
// an exact copy of the source and the other).
func operationRoundTripThrough(t testing.TB, lab *operationLab, finish func(string), abandon func(string)) operationRoundTrip {
	t.Helper()
	abandonedCopy := filepath.Join(lab.directory, "abandoned-copy")
	operationCopyTree(t, lab.source, abandonedCopy)
	// The copy's index records the source's file times; Git's own abort steps
	// do not refresh it before they compare.
	operationStep(t, abandonedCopy, "update-index", "-q", "--refresh")
	snapshot := operationCapture(t, lab.source, lab.base)
	archive := filepath.Join(lab.directory, "checkpoint.hel.zip")
	archiveInput := input()
	archiveInput.Bundle.PrimaryRepository = "repo"
	archiveInput.Repositories = []RepositorySnapshot{snapshot}
	if _, err := WriteArchiveAtomic(archive, &archiveInput); err != nil {
		t.Fatal(err)
	}

	finishedRestored := operationRestoreArchive(t, lab, archive, "finished")
	abandonedRestored := operationRestoreArchive(t, lab, archive, "abandoned")
	before := operationObserve(t, lab.source)
	operationAssertSame(t, operationObserve(t, finishedRestored), before)
	operationAssertSame(t, operationObserve(t, abandonedRestored), before)

	finish(lab.source)
	finish(finishedRestored)
	operationAssertSame(t, operationObserve(t, finishedRestored), operationObserve(t, lab.source))
	abandon(abandonedCopy)
	abandon(abandonedRestored)
	operationAssertSame(t, operationObserve(t, abandonedRestored), operationObserve(t, abandonedCopy))
	return operationRoundTrip{
		finishedSource:    lab.source,
		finishedRestored:  finishedRestored,
		abandonedCopy:     abandonedCopy,
		abandonedRestored: abandonedRestored,
	}
}

func operationParents(repository string) []string {
	return strings.Split(gitLine(repository, "rev-list", "--parents", "-n", "1", "HEAD"), " ")[1:]
}

// Hard-won: #1319: a clone mid-merge could not be checkpointed, so suspend failed forever
// Hard-won: 5a3457a30: Git 2.25 echoed --path-format=absolute, so every clone checkpoint failed to locate its operation state.
func TestACloneStoppedInAConflictedMergeResumesMidMerge(t *testing.T) {
	lab := newOperationLab(t)
	diverged := operationDiverge(t, lab)
	operationStepOK(t, lab.source, "switch", "-q", "main")
	// An rr-cache directory turns rerere on, so the merge records its
	// conflicts there and in MERGE_RR.
	if err := os.Mkdir(filepath.Join(lab.source, ".git/rr-cache"), 0o777); err != nil {
		t.Fatal(err)
	}
	operationDirtyBeforeStarting(t, lab.source)
	operationStepStops(t, lab.source, "merge", "--autostash", "side")
	operationPartlyResolve(t, lab.source)

	checked := operationRoundTripThrough(t, lab,
		func(repository string) {
			operationFinishResolving(t, repository)
			operationStepOK(t, repository, "commit", "--no-edit")
		},
		func(repository string) { operationStepOK(t, repository, "merge", "--abort") },
	)
	if parents := operationParents(checked.finishedRestored); !slices.Equal(parents, []string{diverged.main, diverged.sideTip}) {
		t.Fatalf("parents %q, want %q", parents, []string{diverged.main, diverged.sideTip})
	}
	if head := gitLine(checked.abandonedRestored, "rev-parse", "HEAD"); head != diverged.main {
		t.Fatalf("HEAD %s, want %s", head, diverged.main)
	}
	contents, err := os.ReadFile(filepath.Join(checked.abandonedRestored, "e.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if string(contents) != "autostashed e\n" {
		t.Fatalf("e.txt is %q", contents)
	}
}

func TestACloneStoppedInACherryPickSequenceResumesIt(t *testing.T) {
	lab := newOperationLab(t)
	diverged := operationDiverge(t, lab)
	operationStepOK(t, lab.source, "switch", "-q", "main")
	// The picked commits are then named only by the operation: the second
	// one only by the sequencer's todo list, with an abbreviated id.
	operationStepOK(t, lab.source, "branch", "-q", "-D", "side")
	operationStepStops(t, lab.source, "cherry-pick", diverged.sideFirst, diverged.sideTip)
	operationPartlyResolve(t, lab.source)

	checked := operationRoundTripThrough(t, lab,
		func(repository string) {
			operationFinishResolving(t, repository)
			operationStepOK(t, repository, "cherry-pick", "--continue")
		},
		func(repository string) { operationStepOK(t, repository, "cherry-pick", "--abort") },
	)
	contents, err := os.ReadFile(filepath.Join(checked.finishedRestored, "s.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if string(contents) != "side s\n" {
		t.Fatalf("s.txt is %q", contents)
	}
	if commit := gitLine(checked.finishedRestored, "rev-parse", "HEAD~2"); commit != diverged.main {
		t.Fatalf("HEAD~2 %s, want %s", commit, diverged.main)
	}
	if head := gitLine(checked.abandonedRestored, "rev-parse", "HEAD"); head != diverged.main {
		t.Fatalf("HEAD %s, want %s", head, diverged.main)
	}
}

func TestACloneStoppedInAConflictedRevertResumesIt(t *testing.T) {
	lab := newOperationLab(t)
	reverted := operationCommitChanges(t, lab.source, "main work", operationMainChanges())
	later := operationCommitChanges(t, lab.source, "later work", operationSideChanges())
	operationStepStops(t, lab.source, "revert", reverted)
	operationPartlyResolve(t, lab.source)

	checked := operationRoundTripThrough(t, lab,
		func(repository string) {
			operationFinishResolving(t, repository)
			operationStepOK(t, repository, "revert", "--continue")
		},
		func(repository string) { operationStepOK(t, repository, "revert", "--abort") },
	)
	if parents := operationParents(checked.finishedRestored); !slices.Equal(parents, []string{later}) {
		t.Fatalf("parents %q, want %q", parents, []string{later})
	}
	if head := gitLine(checked.abandonedRestored, "rev-parse", "HEAD"); head != later {
		t.Fatalf("HEAD %s, want %s", head, later)
	}
}

func TestACloneStoppedInARebaseResumesItWithEitherBackend(t *testing.T) {
	for _, backend := range []string{"--merge", "--apply"} {
		lab := newOperationLab(t)
		diverged := operationDiverge(t, lab)
		operationDirtyBeforeStarting(t, lab.source)
		operationStepStops(t, lab.source, "rebase", backend, "--autostash", "main")
		operationPartlyResolve(t, lab.source)

		checked := operationRoundTripThrough(t, lab,
			func(repository string) {
				operationFinishResolving(t, repository)
				// A rebase continues only once nothing is left unstaged.
				operationStepOK(t, repository, "add", "d.txt")
				operationStepOK(t, repository, "rebase", "--continue")
			},
			func(repository string) { operationStepOK(t, repository, "rebase", "--abort") },
		)
		for _, repository := range []string{checked.finishedSource, checked.finishedRestored} {
			if head := gitLine(repository, "symbolic-ref", "HEAD"); head != "refs/heads/side" {
				t.Fatalf("%s: HEAD is %s", backend, head)
			}
			if commit := gitLine(repository, "rev-parse", "HEAD~2"); commit != diverged.main {
				t.Fatalf("%s: HEAD~2 %s, want %s", backend, commit, diverged.main)
			}
		}
		for _, repository := range []string{checked.abandonedCopy, checked.abandonedRestored} {
			if head := gitLine(repository, "rev-parse", "HEAD"); head != diverged.sideTip {
				t.Fatalf("%s: HEAD %s, want %s", backend, head, diverged.sideTip)
			}
		}
	}
}

func TestACloneInTheMiddleOfABisectResumesIt(t *testing.T) {
	lab := newOperationLab(t)
	for number := 1; number <= 6; number++ {
		operationCommitChanges(t, lab.source, fmt.Sprintf("history %d", number),
			[]operationChange{{"h.txt", someText(fmt.Sprintf("%d\n", number))}})
	}
	tip := gitLine(lab.source, "rev-parse", "HEAD")
	operationStepOK(t, lab.source, "bisect", "start")
	operationStepOK(t, lab.source, "bisect", "bad")
	operationStepOK(t, lab.source, "bisect", "good", lab.base)
	operationStepOK(t, lab.source, "bisect", "bad")
	operationUnrelatedWork(t, lab.source)
	log := git(lab.source, "bisect", "log")

	checked := operationRoundTripThrough(t, lab,
		func(repository string) {
			if got := git(repository, "bisect", "log"); string(got) != string(log) {
				t.Fatalf("bisect log %q, want %q", got, log)
			}
			operationStepOK(t, repository, "bisect", "good")
		},
		func(repository string) { operationStepOK(t, repository, "bisect", "reset") },
	)
	if head := gitLine(checked.finishedRestored, "rev-parse", "HEAD"); head == tip {
		t.Fatalf("HEAD stayed at %s", tip)
	}
	if head := gitLine(checked.abandonedRestored, "rev-parse", "HEAD"); head != tip {
		t.Fatalf("HEAD %s, want %s", head, tip)
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
