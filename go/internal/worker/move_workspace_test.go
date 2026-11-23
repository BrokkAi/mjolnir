// Port of the tests in mj-worker/src/move_workspace.rs.

package worker

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func moveTestGit(t *testing.T, root string, args ...string) []byte {
	t.Helper()
	output, err := git(root, args)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

func moveTestGitText(t *testing.T, root string, args ...string) string {
	t.Helper()
	text, err := gitText(root, args)
	if err != nil {
		t.Fatal(err)
	}
	return text
}

func moveTestWrite(t *testing.T, path string, contents []byte) {
	t.Helper()
	if err := os.WriteFile(path, contents, 0o666); err != nil {
		t.Fatal(err)
	}
}

func moveTestInit(t *testing.T, root string) {
	t.Helper()
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	moveTestGit(t, root, "init", "-b", "main")
	moveTestGit(t, root, "config", "user.email", "move@example.invalid")
	moveTestGit(t, root, "config", "user.name", "Move test")
	moveTestWrite(t, filepath.Join(root, ".git/empty-ignore"), nil)
	moveTestGit(t, root, "config", "core.excludesFile", filepath.Join(root, ".git/empty-ignore"))
	moveTestWrite(t, filepath.Join(root, "tracked"), []byte("base\n"))
	moveTestWrite(t, filepath.Join(root, ".gitignore"), []byte("ignored\n"))
	moveTestGit(t, root, "add", ".")
	moveTestGit(t, root, "commit", "-m", "base")
}

func moveTestDigest(t *testing.T, path string) string {
	t.Helper()
	size, hash, err := digest(path)
	if err != nil {
		t.Fatal(err)
	}
	return fmt.Sprintf("%d %s", size, hash)
}

// moveSkipUnlessIgnoredTestsRequested is Rust's #[ignore]: the test runs
// only when MJ_RUN_IGNORED_TESTS=1 asks for it, as `cargo test -- --ignored`
// does.
func moveSkipUnlessIgnoredTestsRequested(t *testing.T, reason string) {
	t.Helper()
	if os.Getenv("MJ_RUN_IGNORED_TESTS") != "1" {
		t.Skip(reason)
	}
}

func TestMoveTransfersAFileLargerThanTheCheckpointPayloadLimit(t *testing.T) {
	moveSkipUnlessIgnoredTestsRequested(t, "acceptance test writes and verifies more than 8 GiB of temporary workspace data")
	directory := t.TempDir()
	source := filepath.Join(directory, "source")
	target := filepath.Join(directory, "target")
	moveTestInit(t, source)
	moveTestGit(t, directory, "clone", source, target)
	size := uint64(8*1024*1024*1024 + 17)
	{
		file, err := os.Create(filepath.Join(source, "large-build-output"))
		if err != nil {
			t.Fatal(err)
		}
		block := bytes.Repeat([]byte{0x5a}, 1024*1024)
		for remaining := size; remaining > 0; {
			count := min(remaining, uint64(len(block)))
			if _, err := file.Write(block[:count]); err != nil {
				t.Fatal(err)
			}
			remaining -= count
		}
		if err := file.Close(); err != nil {
			t.Fatal(err)
		}
	}
	stage := filepath.Join(directory, "move-stage")
	manifest, err := Capture(
		[]core.WorkspaceRepository{{ID: "repo", Root: source}},
		&core.WorkspaceSelection{AcknowledgeLargeTransfer: true},
		stage,
	)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(manifest.Files, func(file core.TransferFile) bool { return file.Bytes > size }) {
		t.Fatalf("%+v", manifest.Files)
	}
	if err := MoveWorkspaceRestore(stage, []core.WorkspaceRepository{{ID: "repo", Root: target}}); err != nil {
		t.Fatal(err)
	}
	info, err := os.Stat(filepath.Join(target, "large-build-output"))
	if err != nil || uint64(info.Size()) != size {
		t.Fatalf("%v %v", info, err)
	}
	if moveTestDigest(t, filepath.Join(source, "large-build-output")) != moveTestDigest(t, filepath.Join(target, "large-build-output")) {
		t.Fatal("the restored file differs")
	}
}

func TestMovePreflightReportsIndexSettingsThatHideTrackedEdits(t *testing.T) {
	directory := t.TempDir()
	moveTestInit(t, directory)
	repositories := []core.WorkspaceRepository{{ID: "repo", Root: directory}}
	refused := func(want string) {
		t.Helper()
		if _, err := MoveWorkspaceInspect(repositories); err == nil || !strings.Contains(err.Error(), want) {
			t.Fatalf("want %q: %v", want, err)
		}
	}
	moveTestGit(t, directory, "update-index", "--assume-unchanged", "tracked")
	moveTestWrite(t, filepath.Join(directory, "tracked"), []byte("hidden edit"))
	refused("assume-unchanged")
	moveTestGit(t, directory, "update-index", "--no-assume-unchanged", "tracked")
	moveTestGit(t, directory, "update-index", "--split-index")
	refused("--no-split-index")
	moveTestGit(t, directory, "update-index", "--no-split-index")
	if _, err := MoveWorkspaceInspect(repositories); err != nil {
		t.Fatal(err)
	}
}

// moveTraceChild names the environment variable that runs the disk-backed
// Move test as the traced child of itself.
const moveTraceChild = "MJ_MOVE_RESTORE_TRACE_CHILD"

// Hard-won: b63fedbf31: Move silently omitted untracked files based on their names.
// Hard-won: 0cffddc616: restoring refs fetched the same bundle once for every ref.
// Hard-won: #1542: filtering transferred base pins also deleted a destination-owned GC root.
func TestDiskBackedMovePreservesGitStateAndSelectedLargeFiles(t *testing.T) {
	if os.Getenv(moveTraceChild) == "" {
		// Scope Git tracing to a child test process; other tests run
		// concurrently.
		directory := t.TempDir()
		trace := filepath.Join(directory, "git-trace.jsonl")
		executable, err := os.Executable()
		if err != nil {
			t.Fatal(err)
		}
		child := exec.Command(executable, "-test.run", "^TestDiskBackedMovePreservesGitStateAndSelectedLargeFiles$", "-test.v")
		child.Env = append(os.Environ(), moveTraceChild+"=1", "GIT_TRACE2_EVENT="+trace)
		output, err := core.RunWithInput(child, nil)
		if err != nil {
			t.Fatal(err)
		}
		if !output.Status.Success() {
			t.Fatalf("%s\n%s", output.Stdout, output.Stderr)
		}
		events, err := os.Open(trace)
		if err != nil {
			t.Fatal(err)
		}
		defer events.Close()
		var commands [][]string
		lines := bufio.NewScanner(events)
		lines.Buffer(nil, 64*1024*1024)
		for lines.Scan() {
			var event struct {
				Event string   `json:"event"`
				Argv  []string `json:"argv"`
			}
			if err := json.Unmarshal(lines.Bytes(), &event); err != nil {
				t.Fatal(err)
			}
			if event.Event == "start" {
				commands = append(commands, event.Argv)
			}
		}
		if err := lines.Err(); err != nil {
			t.Fatal(err)
		}
		unbundles := 0
		for _, args := range commands {
			for index := 0; index+1 < len(args); index++ {
				if args[index] == "bundle" && args[index+1] == "unbundle" {
					unbundles++
					break
				}
			}
			if slices.Contains(args, "fetch") {
				t.Fatalf("Move must not fetch individual saved refs: %v", args)
			}
		}
		if unbundles != 1 {
			t.Fatalf("Move must import the complete bundle exactly once, not %d times", unbundles)
		}
		return
	}
	temp := t.TempDir()
	source := filepath.Join(temp, "source")
	target := filepath.Join(temp, "target")
	moveTestInit(t, source)
	moveTestGit(t, temp, "clone", source, target)
	moveTestWrite(t, filepath.Join(source, "committed-new"), []byte("new commit\n"))
	moveTestGit(t, source, "add", ".")
	moveTestGit(t, source, "commit", "-m", "new head")
	moveTestWrite(t, filepath.Join(source, "tracked"), []byte("stashed\n"))
	moveTestGit(t, source, "stash", "push", "-m", "saved work")
	moveTestWrite(t, filepath.Join(source, "tracked"), []byte("second stash\n"))
	moveTestGit(t, source, "stash", "push", "-m", "another saved change")
	moveTestGit(t, source, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/heads/main")
	head := moveTestGitText(t, source, "rev-parse", "HEAD")
	tree, err := checkpoint.CaptureWorktreeTree(checkpoint.SystemGit{}, source)
	if err != nil {
		t.Fatal(err)
	}
	session := "moving"
	point, err := checkpoint.RecordReviewPoint(checkpoint.SystemGit{}, source, &checkpoint.ReviewPoint{
		Tree:    tree,
		Head:    &head,
		Session: &session,
	})
	if err != nil {
		t.Fatal(err)
	}
	for _, ref := range [][2]string{
		{"refs/mj/reviewed/" + tree, head},
		{"refs/mj/overridden/" + tree, head},
		{"refs/mj/session-start/session/baseline", head},
		{"refs/mj/review-baseline", point},
		{"refs/mj/worktrees/linked/review-baseline", tree},
	} {
		moveTestGit(t, source, "update-ref", ref[0], ref[1])
	}
	moveTestGit(t, source, "tag", "-a", "move-tag", "-m", "saved tag")
	moveTestGit(t, target, "update-ref", "refs/mj/replaced", "HEAD")
	moveTestGit(t, source, "update-ref", "refs/mj/replaced/child", "HEAD")
	moveTestGit(t, target, "symbolic-ref", "refs/mj/destination-only", "refs/heads/main")
	head = moveTestGitText(t, source, "rev-parse", "HEAD")
	var scaleRefs strings.Builder
	for index := range 2048 {
		fmt.Fprintf(&scaleRefs, "update refs/remotes/origin/move-scale/%04d %s\n", index, head)
	}
	if scaleRefs.Len() <= 64*1024 {
		t.Fatal(scaleRefs.Len())
	}
	if _, err := gitWithInput(source, []string{"update-ref", "--stdin"}, []byte(scaleRefs.String())); err != nil {
		t.Fatal(err)
	}
	moveTestWrite(t, filepath.Join(source, "tracked"), []byte("staged\n"))
	moveTestGit(t, source, "add", "tracked")
	moveTestWrite(t, filepath.Join(source, "tracked"), []byte("unstaged\n"))
	body := bytes.Repeat([]byte{'x'}, 512*1024+17)
	moveTestWrite(t, filepath.Join(source, "large"), body)
	sparse, err := os.Create(filepath.Join(source, "sparse"))
	if err != nil {
		t.Fatal(err)
	}
	if err := sparse.Truncate(2 * 1024 * 1024); err != nil {
		t.Fatal(err)
	}
	_ = sparse.Close()
	moveTestWrite(t, filepath.Join(source, "excluded"), []byte("keep at source"))
	moveTestWrite(t, filepath.Join(source, "ignored"), []byte("ignored"))
	moveTestWrite(t, filepath.Join(source, ".env"), []byte("private"))
	moveTestWrite(t, filepath.Join(source, "config.json"), []byte(`{"flow":4}`))
	repositories := []core.WorkspaceRepository{{ID: "repo", Root: source}}
	assessment, err := MoveWorkspaceInspect(repositories)
	if err != nil {
		t.Fatal(err)
	}
	if assessment.IgnoredFiles != 1 {
		t.Fatal(assessment.IgnoredFiles)
	}
	selection := core.WorkspaceSelection{Exclusions: []core.WorkspacePath{{Repository: "repo", Path: "excluded"}}}
	stage := filepath.Join(temp, "stage")
	if _, err := Capture(repositories, &selection, stage); err != nil {
		t.Fatal(err)
	}
	hooks := filepath.Join(target, ".git/hooks")
	if err := os.MkdirAll(hooks, 0o777); err != nil {
		t.Fatal(err)
	}
	moveTestGit(t, target, "config", "core.hooksPath", hooks)
	hook := filepath.Join(hooks, "reference-transaction")
	moveTestWrite(t, hook, []byte(`#!/bin/sh
set -eu
if [ "$1" = prepared ]; then
    cat > .git/move-ref-input
    if grep -q ' refs/remotes/origin/move-scale/' .git/move-ref-input; then
        [ "$(grep -c ' refs/remotes/origin/move-scale/' .git/move-ref-input)" -eq 2048 ] || exit 1
        cat .git/move-ref-input >> .git/move-ref-batches
        printf 'batch\n' >> .git/move-ref-batch-count
    fi
fi
`))
	if runtime.GOOS != "windows" {
		if err := os.Chmod(hook, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	// Pins already in the destination belong to the user's repository.
	blobOutput, err := gitWithInput(target, []string{"hash-object", "-w", "--stdin"}, []byte("pin"))
	if err != nil {
		t.Fatal(err)
	}
	blob := strings.TrimSpace(string(blobOutput))
	existing := []string{
		"refs/mj/worktrees/other/review-baseline",
		"refs/hel/review-baseline",
		core.HistoryCutoffBaseRef,
	}
	for _, pin := range existing {
		moveTestGit(t, target, "update-ref", pin, blob)
	}
	if err := MoveWorkspaceRestore(stage, []core.WorkspaceRepository{{ID: "repo", Root: target}}); err != nil {
		t.Fatal(err)
	}
	for _, pin := range existing {
		if got := strings.TrimSpace(moveTestGitText(t, target, "rev-parse", pin)); got != blob {
			t.Fatalf("%s: %s", pin, got)
		}
	}
	if info, err := os.Stat(filepath.Join(target, ".git/move-ref-batches")); err != nil || info.Size() <= 64*1024 {
		t.Fatalf("%v %v", info, err)
	}
	if count, err := os.ReadFile(filepath.Join(target, ".git/move-ref-batch-count")); err != nil || string(count) != "batch\n" {
		t.Fatalf("%q %v", count, err)
	}
	for _, args := range [][]string{
		{"rev-parse", "HEAD"},
		{"status", "--porcelain=v1", "--untracked-files=no"},
		{"ls-files", "--stage"},
		{"stash", "list"},
		{"show", "stash@{1}:tracked"},
		{"diff", "--binary"},
		{"diff", "--cached", "--binary"},
	} {
		if !bytes.Equal(moveTestGit(t, source, args...), moveTestGit(t, target, args...)) {
			t.Fatalf("%v differs", args)
		}
	}
	// Transferred refs agree except obsolete review pins and the managed
	// base pin, whose ownership stays with each checkout.
	refsWithoutPins := func(root string) string {
		var kept []string
		for _, line := range captureLines(moveTestGitText(t, root, "for-each-ref", "--format=%(refname) %(objectname) %(symref)")) {
			fields := strings.Fields(line)
			if strings.Contains(line, "review-baseline") || (len(fields) > 0 && fields[0] == core.HistoryCutoffBaseRef) {
				continue
			}
			kept = append(kept, line)
		}
		return strings.Join(kept, "\n")
	}
	if refsWithoutPins(source) != refsWithoutPins(target) {
		t.Fatalf("refs differ:\n%s\n---\n%s", refsWithoutPins(source), refsWithoutPins(target))
	}
	for _, pin := range []string{"refs/mj/review-baseline", "refs/mj/worktrees/linked/review-baseline"} {
		if len(moveTestGit(t, source, "for-each-ref", pin)) == 0 {
			t.Fatalf("%s is missing at the source", pin)
		}
		if len(moveTestGit(t, target, "for-each-ref", pin)) != 0 {
			t.Fatalf("%s was carried to the target", pin)
		}
	}
	if large, err := os.ReadFile(filepath.Join(target, "large")); err != nil || !bytes.Equal(large, body) {
		t.Fatalf("large: %v", err)
	}
	if moveTestDigest(t, filepath.Join(source, "sparse")) != moveTestDigest(t, filepath.Join(target, "sparse")) {
		t.Fatal("the sparse file differs")
	}
	if _, err := os.Stat(filepath.Join(target, "excluded")); err == nil {
		t.Fatal("the excluded file was carried")
	}
	if _, err := os.Stat(filepath.Join(source, "excluded")); err != nil {
		t.Fatal("the excluded file left the source")
	}
	if _, err := os.Stat(filepath.Join(target, "ignored")); err == nil {
		t.Fatal("the ignored file was carried")
	}
	if env, err := os.ReadFile(filepath.Join(target, ".env")); err != nil || string(env) != "private" {
		t.Fatalf("%q %v", env, err)
	}
	if config, err := os.ReadFile(filepath.Join(target, "config.json")); err != nil || string(config) != `{"flow":4}` {
		t.Fatalf("%q %v", config, err)
	}
	moveTestWrite(t, filepath.Join(stage, "repo/staged.patch"), []byte("corrupt"))
	if _, err := Verify(stage); err == nil || !strings.Contains(err.Error(), "staged.patch") {
		t.Fatal(err)
	}
}
