// Port of the tests in mj-worker-review/src/hook.rs.

//go:build unix

package workerreview

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func hookTestPush(t *testing.T, root string, gate bool, refs ...string) checkpoint.GitOutput {
	t.Helper()
	value := "0"
	if gate {
		value = "1"
	}
	output, err := checkpoint.SystemGit{}.Run(root, &checkpoint.GitCommand{
		Arguments: append([]string{"push", "origin"}, refs...),
		Env:       [][2]string{{"MJ_REVIEW_GATE", value}},
	})
	if err != nil {
		t.Fatal(err)
	}
	return output
}

func hookTestReviewed(t *testing.T, root string) {
	t.Helper()
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	head := testsGit(root, "rev-parse", "HEAD")
	pushTestRecordReviewed(t, root, tree, head)
}

// hookTestCheckout is the canonical checkout of a pushTestRepository.
func hookTestCheckout(t *testing.T, temp string) string {
	t.Helper()
	root, err := filepath.EvalSymlinks(filepath.Join(temp, "checkout"))
	if err != nil {
		t.Fatal(err)
	}
	return root
}

func hookTestInstall(t *testing.T, root string) HookInstall {
	t.Helper()
	installed, err := InstallPrePushHook(checkpoint.SystemGit{}, root)
	if err != nil {
		t.Fatal(err)
	}
	return installed
}

func hookTestExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

func hookTestRead(t *testing.T, path string) string {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(contents)
}

func TestHookGatesPushesByTreeAllowsDeletionAndIsIdempotent(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallInstalled{}) {
		t.Fatalf("%#v", got)
	}
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallRewritten{}) {
		t.Fatalf("%#v", got)
	}
	if hookTestExists(filepath.Join(root, ".git/hooks/pre-push.before-mj")) {
		t.Fatal("a backup of Mjolnir's own hook was kept")
	}
	pushTestCommit(t, root, "unreviewed")
	rejected := hookTestPush(t, root, true, "HEAD:refs/heads/feature")
	if rejected.Status == 0 {
		t.Fatal("an unreviewed push was accepted")
	}
	stderr := captureLossy(rejected.Stderr)
	for _, want := range []string{
		"mj: HEAD (tree ",
		"has not been reviewed.",
		fmt.Sprintf("call the mj-review request_review tool for the worktree \"%s\", wait for the verdict message, then push again.", root),
	} {
		if !strings.Contains(stderr, want) {
			t.Fatalf("missing %q in %s", want, stderr)
		}
	}
	hookTestReviewed(t, root)
	if status := hookTestPush(t, root, true, "HEAD:refs/heads/feature").Status; status != 0 {
		t.Fatal(status)
	}
	pushTestCommit(t, root, "gate off")
	if status := hookTestPush(t, root, false, "HEAD:refs/heads/feature").Status; status != 0 {
		t.Fatal(status)
	}
	if status := hookTestPush(t, root, true, ":refs/heads/feature").Status; status != 0 {
		t.Fatal(status)
	}
}

// An overridden tree may be pushed, and a hook written before overridden
// refs existed is replaced when the hook is installed again.
func TestHookAcceptsAnOverriddenTreeAndAReinstallReplacesTheOldScript(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	hookTestInstall(t, root)
	hook := filepath.Join(root, ".git/hooks/pre-push")
	old := strings.ReplaceAll(script, ` && ! git show-ref --verify --quiet "refs/mj/overridden/$tree"`, "")
	if old == script {
		t.Fatal("the old script is the current one")
	}
	pushTestWrite(t, hook, old)
	head := pushTestCommit(t, root, "overridden")
	tree := testsGit(root, "rev-parse", "HEAD^{tree}")
	if err := checkpoint.RecordOverriddenTree(checkpoint.SystemGit{}, root, tree, head); err != nil {
		t.Fatal(err)
	}
	if hookTestPush(t, root, true, "HEAD:refs/heads/feature").Status == 0 {
		t.Fatal("the old hook accepted an overridden tree")
	}
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallRewritten{}) {
		t.Fatalf("%#v", got)
	}
	if got := hookTestRead(t, hook); got != script {
		t.Fatal(got)
	}
	if hookTestExists(filepath.Join(root, ".git/hooks/pre-push.before-mj")) {
		t.Fatal("a backup of Mjolnir's own hook was kept")
	}
	if status := hookTestPush(t, root, true, "HEAD:refs/heads/feature").Status; status != 0 {
		t.Fatal(status)
	}
}

func TestPreservedHookGetsArgumentsStdinAndControlsExitStatus(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	hooks := filepath.Join(root, ".git/hooks")
	pushTestWrite(t, filepath.Join(hooks, "pre-push"),
		"#!/bin/sh\nprintf '%s\\n' \"$@\" > hook-arguments\ncat > hook-input\nexit 23\n")
	if err := os.Chmod(filepath.Join(hooks, "pre-push"), 0o755); err != nil {
		t.Fatal(err)
	}
	hookTestInstall(t, root)
	hookTestInstall(t, root)
	if !hookTestExists(filepath.Join(hooks, "pre-push.before-mj")) {
		t.Fatal("the user's hook was not preserved")
	}
	head := pushTestCommit(t, root, "later")
	if hookTestPush(t, root, true, "HEAD:refs/heads/feature").Status == 0 {
		t.Fatal("an unreviewed push was accepted")
	}
	if hookTestExists(filepath.Join(root, "hook-input")) {
		t.Fatal("the user's hook ran for a refused push")
	}
	hookTestReviewed(t, root)
	if hookTestPush(t, root, true, "HEAD:refs/heads/feature").Status == 0 {
		t.Fatal("the user's hook did not control the exit status")
	}
	if input := hookTestRead(t, filepath.Join(root, "hook-input")); !strings.HasPrefix(input, fmt.Sprintf("HEAD %s refs/heads/feature ", head)) {
		t.Fatal(input)
	}
	if arguments := hookTestRead(t, filepath.Join(root, "hook-arguments")); !strings.HasPrefix(arguments, "origin\n") {
		t.Fatal(arguments)
	}
	if err := os.Remove(filepath.Join(root, "hook-input")); err != nil {
		t.Fatal(err)
	}
	if hookTestPush(t, root, false, "HEAD:refs/heads/feature").Status == 0 {
		t.Fatal("the user's hook did not control the exit status")
	}
	if !hookTestExists(filepath.Join(root, "hook-input")) {
		t.Fatal("the user's hook did not run with the gate off")
	}
	// Git collapses hook failures to status 1. Call the script directly
	// as well to check that it preserves the chained hook's exact status.
	input := strings.Repeat("ref line\n", 20_000)
	temporary := filepath.Join(root, "hook-temporary")
	if err := os.Mkdir(temporary, 0o777); err != nil {
		t.Fatal(err)
	}
	command := exec.Command(filepath.Join(hooks, "pre-push"))
	command.Dir = root
	command.Env = append(os.Environ(), "MJ_REVIEW_GATE=0", "TMPDIR="+temporary)
	output, err := core.RunWithInput(command, []byte(input))
	if err != nil {
		t.Fatal(err)
	}
	if code := output.Status.ExitCode(); code != 23 {
		t.Fatal(code)
	}
	if got := hookTestRead(t, filepath.Join(root, "hook-input")); got != input {
		t.Fatalf("the chained hook read %d bytes of %d", len(got), len(input))
	}
	if entries, err := os.ReadDir(temporary); err != nil || len(entries) != 0 {
		t.Fatalf("%v %v", entries, err)
	}
}

func TestConfiguredHooksPathAndLinkedWorktreeUseGitHookLocations(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	external := filepath.Join(temp, "custom hooks")
	testsGit(root, "config", "core.hooksPath", external)
	hookTestInstall(t, root)
	if !hookTestExists(filepath.Join(external, "pre-push")) {
		t.Fatal("the hook is not in the configured directory")
	}
	pushTestCommit(t, root, "later")
	if hookTestPush(t, root, true, "HEAD:refs/heads/custom").Status == 0 {
		t.Fatal("an unreviewed push was accepted")
	}
	testsGit(root, "config", "--unset", "core.hooksPath")
	linked := filepath.Join(temp, "linked")
	testsGit(root, "worktree", "add", "-qb", "linked", linked)
	if info, err := os.Stat(filepath.Join(linked, ".git")); err != nil || !info.Mode().IsRegular() {
		t.Fatalf("%v %v", info, err)
	}
	hookTestInstall(t, linked)
	if !hookTestExists(filepath.Join(root, ".git/hooks/pre-push")) {
		t.Fatal("the linked worktree's hook is not in the common directory")
	}
	if hookTestPush(t, linked, true, "HEAD:refs/heads/linked").Status == 0 {
		t.Fatal("an unreviewed push was accepted")
	}
	hookTestReviewed(t, linked)
	if status := hookTestPush(t, linked, true, "HEAD:refs/heads/linked").Status; status != 0 {
		t.Fatal(status)
	}
}

// Hard-won: before-push review: installing into tracked Husky hooks dirtied every review capture.
func TestTrackedWorktreeHooksAreRefusedWithoutFileChanges(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	hooks := filepath.Join(root, ".husky")
	if err := os.Mkdir(hooks, 0o777); err != nil {
		t.Fatal(err)
	}
	pushTestWrite(t, filepath.Join(hooks, "pre-push"), "#!/bin/sh\nexit 0\n")
	testsGit(root, "add", ".husky/pre-push")
	testsGit(root, "commit", "-qm", "tracked Husky hook")
	testsGit(root, "config", "core.hooksPath", ".husky")
	before := hookTestRead(t, filepath.Join(hooks, "pre-push"))
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallRefusedWorkTreeHooksDirectory{Path: hooks}) {
		t.Fatalf("%#v", got)
	}
	if got := hookTestRead(t, filepath.Join(hooks, "pre-push")); got != before {
		t.Fatal(got)
	}
	if entries, err := os.ReadDir(hooks); err != nil || len(entries) != 1 {
		t.Fatalf("%v %v", entries, err)
	}
	if status := testsGit(root, "status", "--porcelain", "--untracked-files=all"); status != "" {
		t.Fatal(status)
	}
	head := testsGit(root, "rev-parse", "HEAD")
	if _, err := CapturePushableCommits(checkpoint.SystemGit{}, root, head); err != nil {
		t.Fatal(err)
	}
	alias := filepath.Join(temp, "linked-hooks")
	if err := os.Symlink(hooks, alias); err != nil {
		t.Fatal(err)
	}
	testsGit(root, "config", "core.hooksPath", alias)
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallRefusedWorkTreeHooksDirectory{Path: hooks}) {
		t.Fatalf("%#v", got)
	}
	// Refuse untracked pollution before even creating the directory.
	testsGit(root, "config", "core.hooksPath", ".untracked-hooks")
	untracked := filepath.Join(root, ".untracked-hooks")
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallRefusedWorkTreeHooksDirectory{Path: untracked}) {
		t.Fatalf("%#v", got)
	}
	if hookTestExists(untracked) {
		t.Fatal("the refused hooks directory was created")
	}
}

func TestIgnoredWorktreeHooksInstallWithoutDirtyingTheCheckout(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	hooks := filepath.Join(root, ".husky/_")
	if err := os.MkdirAll(hooks, 0o777); err != nil {
		t.Fatal(err)
	}
	pushTestWrite(t, filepath.Join(hooks, ".gitignore"), "*\n")
	testsGit(root, "config", "core.hooksPath", ".husky/_")
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallInstalled{}) {
		t.Fatalf("%#v", got)
	}
	if !hookTestExists(filepath.Join(hooks, "pre-push")) {
		t.Fatal("the hook was not installed")
	}
	if status := testsGit(root, "status", "--porcelain", "--untracked-files=all"); status != "" {
		t.Fatal(status)
	}
	pushTestCommit(t, root, "unreviewed")
	if hookTestPush(t, root, true, "HEAD:refs/heads/feature").Status == 0 {
		t.Fatal("an unreviewed push was accepted")
	}
	hookTestReviewed(t, root)
	if status := hookTestPush(t, root, true, "HEAD:refs/heads/feature").Status; status != 0 {
		t.Fatal(status)
	}
}

func TestInterruptedBackupOfTheSameFileCanFinish(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	hooks := filepath.Join(root, ".git/hooks")
	pushTestWrite(t, filepath.Join(hooks, "pre-push"), "user hook")
	if err := os.Link(filepath.Join(hooks, "pre-push"), filepath.Join(hooks, "pre-push.before-mj")); err != nil {
		t.Fatal(err)
	}
	if got := hookTestInstall(t, root); got != HookInstall(HookInstallInstalled{}) {
		t.Fatalf("%#v", got)
	}
	if got := hookTestRead(t, filepath.Join(hooks, "pre-push.before-mj")); got != "user hook" {
		t.Fatal(got)
	}
}

func TestTakenBackupRefusesWithoutChangingEitherHook(t *testing.T) {
	temp := pushTestRepository(t)
	root := hookTestCheckout(t, temp)
	hooks := filepath.Join(root, ".git/hooks")
	pushTestWrite(t, filepath.Join(hooks, "pre-push"), "user hook")
	pushTestWrite(t, filepath.Join(hooks, "pre-push.before-mj"), "earlier hook")
	want := HookInstallRefusedExistingBackup{Path: filepath.Join(hooks, "pre-push.before-mj")}
	if got := hookTestInstall(t, root); got != HookInstall(want) {
		t.Fatalf("%#v", got)
	}
	if got := hookTestRead(t, filepath.Join(hooks, "pre-push")); got != "user hook" {
		t.Fatal(got)
	}
	if got := hookTestRead(t, filepath.Join(hooks, "pre-push.before-mj")); got != "earlier hook" {
		t.Fatal(got)
	}
}
