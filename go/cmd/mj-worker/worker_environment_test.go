// Port of mj-worker-bin/tests/worker_environment.rs: real worker entrypoints
// must clean inherited state before invoking Git.

//go:build unix

package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// executed is a command's output, or why it could not run.
type executed struct {
	output core.CommandOutput
	err    error
}

// executeInBackground runs command under a bounded executor on its own
// goroutine, as the Rust tests run one on a thread.
func executeInBackground(command core.CommandSpec, timeout time.Duration) <-chan executed {
	done := make(chan executed, 1)
	go func() {
		output, err := core.NewBoundedProcessExecutor(timeout).Execute(&command)
		done <- executed{output, err}
	}()
	return done
}

// finished reports whether a background command has ended, keeping its
// result for a later receive.
func finished(done <-chan executed, result *executed, received *bool) bool {
	if *received {
		return true
	}
	select {
	case *result = <-done:
		*received = true
		return true
	default:
		return false
	}
}

type pushFixture struct {
	directory  string
	root       string
	repository string
	remote     string
	git        string
}

func newPushFixture(t *testing.T) *pushFixture {
	t.Helper()
	directory := t.TempDir()
	fixture := &pushFixture{
		directory:  directory,
		root:       filepath.Join(directory, "worker with spaces"),
		repository: filepath.Join(directory, "repository"),
		remote:     filepath.Join(directory, "remote.git"),
	}
	bin := filepath.Join(directory, "tools")
	home := filepath.Join(directory, "home")
	for _, path := range []string{fixture.root, fixture.repository, bin, home} {
		if err := os.MkdirAll(path, 0o777); err != nil {
			t.Fatal(err)
		}
	}
	lookup := core.NewCommandSpec("sh", []string{"-c", "command -v git"})
	output, err := core.NewBoundedProcessExecutor(15 * time.Second).Execute(&lookup)
	if err != nil || output.Status != 0 {
		t.Fatalf("find git: %v %v", err, output)
	}
	fixture.git = strings.TrimSpace(string(output.Stdout))
	if err := os.WriteFile(filepath.Join(home, ".gitconfig"), []byte("[user]\nname = Session User\nemail = session@example.invalid\n[commit]\ngpgsign = false\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	environment := map[string]string{
		"HOME":                home,
		"PATH":                bin + ":/usr/bin:/bin",
		"GIT_CONFIG_NOSYSTEM": "1",
		"GH_TOKEN":            "stale-target-token",
		"GITHUB_TOKEN":        "other-stale-target-token",
		"GIT_CONFIG_COUNT":    "2",
		"GIT_CONFIG_KEY_0":    "user.email",
		"GIT_CONFIG_VALUE_0":  "target@example.invalid",
		"GIT_CONFIG_KEY_1":    "credential.https://github.com.helper",
		"GIT_CONFIG_VALUE_1":  "invalid-target-helper",
	}
	var config core.WorkerLaunchConfig
	decodeJSON(t, map[string]any{
		"session_id": stopTestSession, "harness": "codex",
		"bridge_command": "/unused-harness", "bridge_args": []string{}, "environment": map[string]any{},
		"target_environment": environment, "cwd": fixture.repository,
		"execution_policy": "configured_approvals",
	}, &config)
	if err := config.Write(filepath.Join(fixture.root, "launch.json")); err != nil {
		t.Fatal(err)
	}
	if err := worker.ConfigureGithubCLI(fixture.root, environment, nil, nil, false); err != nil {
		t.Fatal(err)
	}
	if err := core.RemoveGithubToken(filepath.Join(fixture.root, "github-token")); err != nil {
		t.Fatal(err)
	}
	core.InstallFakeCommand(bin, "gh", `#!/bin/sh
set -eu
[ "$1" = auth ] && [ "$2" = git-credential ]
cat >/dev/null
if [ "${3-}" = get ] && [ -n "${GH_TOKEN:-}" ]; then
    printf 'username=x-access-token\npassword=%s\n' "$GH_TOKEN"
fi
`)
	fixture.runGit(t, fixture.repository, "init", "-q")
	fixture.runGit(t, filepath.Dir(fixture.remote), "init", "--bare", "-q", fixture.remote)
	fixture.runGit(t, fixture.repository,
		"-c", "user.name=Test",
		"-c", "user.email=test@example.invalid",
		"-c", "commit.gpgsign=false",
		"commit", "--allow-empty", "-qm", "initial")
	fixture.runGit(t, fixture.repository, "remote", "add", "origin", fixture.remote)
	quote := core.PosixQuote
	hook := filepath.Join(fixture.repository, ".git", "hooks", "pre-push")
	script := fmt.Sprintf(`#!/bin/sh
set -eu
test "${GIT_CONFIG_GLOBAL:-}" = %[1]s || { echo 'missing session Git config' >&2; exit 1; }
test -z "${GH_TOKEN:-}${GITHUB_TOKEN:-}"
test -z "${GIT_CONFIG_COUNT:-}${GIT_CONFIG_KEY_0:-}${GIT_CONFIG_VALUE_0:-}"
test "$(%[2]s config --get user.name)" = 'Session User'
test "$(%[2]s config --get user.email)" = 'target@example.invalid'
credentials=$(printf 'protocol=https\nhost=github.com\n\n' | %[2]s credential fill)
expected=$(cat %[3]s)
test "$credentials" = "$(printf 'protocol=https\nhost=github.com\nusername=x-access-token\npassword=%%s' "$expected")"
`,
		quote(filepath.Join(fixture.root, "gitconfig")),
		quote(fixture.git),
		quote(filepath.Join(directory, "expected-token")))
	core.InstallFakeCommand(filepath.Dir(hook), "pre-push", script)
	return fixture
}

func decodeJSON(t *testing.T, value any, target any) {
	t.Helper()
	data, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, target); err != nil {
		t.Fatal(err)
	}
}

func (f *pushFixture) runGit(t *testing.T, cwd string, args ...string) []byte {
	t.Helper()
	command := core.NewCommandSpec(f.git, args)
	command.Cwd = &cwd
	command.ClearEnv = true
	command.Env = map[string]string{
		"PATH":                "/usr/bin:/bin",
		"HOME":                f.directory,
		"GIT_CONFIG_GLOBAL":   "/dev/null",
		"GIT_CONFIG_NOSYSTEM": "1",
	}
	output, err := core.NewBoundedProcessExecutor(15 * time.Second).Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("git %v: %s", args, output.Stderr)
	}
	return output.Stdout
}

func (f *pushFixture) push(t *testing.T, branch string) core.CommandOutput {
	t.Helper()
	command := core.NewCommandSpec(workerExecutable, []string{
		"worker", "push-branch",
		"--root", f.root,
		"--repository", f.repository,
		"--branch", branch,
	})
	command.Env = map[string]string{
		"HOME":               "/wrong-launcher-home",
		"PATH":               "/wrong-launcher-path",
		"GIT_DIR":            "/wrong-launcher-repository",
		"GIT_WORK_TREE":      "/wrong-launcher-worktree",
		"GIT_CONFIG_GLOBAL":  "/wrong-launcher-config",
		"GH_TOKEN":           "stale-launcher-token",
		"GITHUB_TOKEN":       "other-stale-launcher-token",
		"GIT_CONFIG_COUNT":   "1",
		"GIT_CONFIG_KEY_0":   "credential.helper",
		"GIT_CONFIG_VALUE_0": "invalid-launcher-helper",
	}
	output, err := core.NewBoundedProcessExecutor(15 * time.Second).Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

// gitConfigGet reads key through the generated global config with nothing
// else configured.
func gitConfigGet(t *testing.T, directory, globalConfig, key string) string {
	t.Helper()
	command := core.NewCommandSpec("git", []string{"config", "--get", key})
	command.Cwd = &directory
	command.ClearEnv = true
	command.Env = map[string]string{
		"HOME":                "/root",
		"PATH":                "/usr/bin:/bin",
		"GIT_CONFIG_GLOBAL":   globalConfig,
		"GIT_CONFIG_NOSYSTEM": "1",
	}
	output, err := core.NewBoundedProcessExecutor(15 * time.Second).Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("git config --get %s: %s", key, output.Stderr)
	}
	return strings.TrimSpace(string(output.Stdout))
}

func TestSessionGitConfigUsesSavedHomeAfterHarnessHomeChanges(t *testing.T) {
	directory := t.TempDir()
	root := filepath.Join(directory, "worker")
	home := filepath.Join(directory, "container-home")
	for _, path := range []string{root, home} {
		if err := os.MkdirAll(path, 0o777); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(home, ".gitconfig"),
		[]byte("[user]\n\tname = Session User\n\temail = session@example.invalid\n[pull]\n\trebase = false\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	environment := map[string]string{"HOME": "/root", "PATH": "/usr/bin:/bin"}
	if err := worker.ConfigureGithubCLI(root, environment, &home, nil, false); err != nil {
		t.Fatal(err)
	}

	globalConfig := environment["GIT_CONFIG_GLOBAL"]
	if globalConfig == "" {
		t.Fatal("no GIT_CONFIG_GLOBAL")
	}
	resumed := map[string]string{
		"HOME":              "/root",
		"PATH":              "/usr/bin:/bin",
		"GIT_CONFIG_GLOBAL": globalConfig,
	}
	if err := worker.ConfigureGithubCLI(root, resumed, &home, nil, false); err != nil {
		t.Fatal(err)
	}
	if resumed["GIT_CONFIG_GLOBAL"] != globalConfig {
		t.Fatalf("GIT_CONFIG_GLOBAL became %q", resumed["GIT_CONFIG_GLOBAL"])
	}

	generated, err := os.ReadFile(globalConfig)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(generated), filepath.Join(home, ".gitconfig")) ||
		strings.Contains(string(generated), "~/.gitconfig") ||
		strings.Contains(string(generated), "~/.config/git/config") {
		t.Fatalf("generated config:\n%s", generated)
	}

	for _, check := range [][2]string{
		{"user.name", "Session User"},
		{"user.email", "session@example.invalid"},
		{"pull.rebase", "false"},
	} {
		if value := gitConfigGet(t, directory, globalConfig, check[0]); value != check[1] {
			t.Fatalf("%s = %q", check[0], value)
		}
	}
}

func TestSessionGitConfigIncludesExplicitContainerConfigAfterHomeChanges(t *testing.T) {
	directory := t.TempDir()
	root := filepath.Join(directory, "worker")
	home := filepath.Join(directory, "container-home")
	for _, path := range []string{root, home} {
		if err := os.MkdirAll(path, 0o777); err != nil {
			t.Fatal(err)
		}
	}
	inheritedConfig := filepath.Join(home, ".gitconfig")
	if err := os.WriteFile(inheritedConfig,
		[]byte("[user]\n\tname = Session User\n\temail = session@example.invalid\n[pull]\n\trebase = false\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	originalGlobal := filepath.Join(directory, "original-global.gitconfig")
	if err := os.WriteFile(originalGlobal, []byte("[pull]\n\trebase = true\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	environment := map[string]string{
		"HOME":              "/root",
		"PATH":              "/usr/bin:/bin",
		"GIT_CONFIG_GLOBAL": originalGlobal,
	}
	rootHome := "/root"
	if err := worker.ConfigureGithubCLI(root, environment, &rootHome, &inheritedConfig, false); err != nil {
		t.Fatal(err)
	}

	globalConfig := environment["GIT_CONFIG_GLOBAL"]
	resumed := map[string]string{
		"HOME":                          "/root",
		"PATH":                          "/usr/bin:/bin",
		"GIT_CONFIG_GLOBAL":             globalConfig,
		"MJ_ORIGINAL_GIT_CONFIG_GLOBAL": originalGlobal,
	}
	if err := worker.ConfigureGithubCLI(root, resumed, &rootHome, &inheritedConfig, false); err != nil {
		t.Fatal(err)
	}
	if resumed["GIT_CONFIG_GLOBAL"] != globalConfig {
		t.Fatalf("GIT_CONFIG_GLOBAL became %q", resumed["GIT_CONFIG_GLOBAL"])
	}

	generated, err := os.ReadFile(globalConfig)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(generated), inheritedConfig) {
		t.Fatalf("generated config:\n%s", generated)
	}

	for _, check := range [][2]string{
		{"user.name", "Session User"},
		{"user.email", "session@example.invalid"},
		{"pull.rebase", "false"},
	} {
		if value := gitConfigGet(t, directory, globalConfig, check[0]); value != check[1] {
			t.Fatalf("%s = %q", check[0], value)
		}
	}
}

func TestBranchExportUsesSessionAuthAfterCleanReexec(t *testing.T) {
	fixture := newPushFixture(t)
	for _, step := range [][2]string{
		{"first-session-token", "review/first"},
		{"rotated-session-token", "review/rotated"},
	} {
		token, branch := step[0], step[1]
		if _, err := core.WriteGithubToken(filepath.Join(fixture.root, "github-token"), []byte(token)); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(fixture.directory, "expected-token"), []byte(token), 0o666); err != nil {
			t.Fatal(err)
		}
		type snapshot struct {
			path     string
			bytes    []byte
			modified time.Time
		}
		var setup []snapshot
		for _, name := range []string{"gitconfig", "gitconfig-inherited", filepath.Join("bin", "gh"), "github-token"} {
			path := filepath.Join(fixture.root, name)
			data, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			info, err := os.Stat(path)
			if err != nil {
				t.Fatal(err)
			}
			setup = append(setup, snapshot{path, data, info.ModTime()})
		}
		output := fixture.push(t, branch)
		if output.Status != 0 {
			t.Fatalf("%s", output.Stderr)
		}
		var pushed checkpoint.PushedBranch
		if err := json.Unmarshal(output.Stdout, &pushed); err != nil {
			t.Fatalf("%v: %s", err, output.Stdout)
		}
		if pushed.Branch != branch {
			t.Fatalf("pushed %+v", pushed)
		}
		if !bytes.Equal(
			fixture.runGit(t, fixture.remote, "rev-parse", "refs/heads/"+branch),
			fixture.runGit(t, fixture.repository, "rev-parse", "HEAD"),
		) {
			t.Fatal("the remote branch is not HEAD")
		}
		if strings.Contains(string(output.Stdout), token) || strings.Contains(string(output.Stderr), token) {
			t.Fatal("the token leaked into the output")
		}
		for _, file := range setup {
			data, err := os.ReadFile(file.path)
			if err != nil || !bytes.Equal(data, file.bytes) {
				t.Fatalf("%s changed", file.path)
			}
			if info, err := os.Stat(file.path); err != nil || !info.ModTime().Equal(file.modified) {
				t.Fatalf("%s was rewritten", file.path)
			}
		}
	}
	if err := core.RemoveGithubToken(filepath.Join(fixture.root, "github-token")); err != nil {
		t.Fatal(err)
	}
	output := fixture.push(t, "review/removed")
	if output.Status == 0 {
		t.Fatal("a push without a token succeeded")
	}
	stderr := string(output.Stderr)
	if !strings.Contains(stderr, "could not read Username") {
		t.Fatal(stderr)
	}
	if strings.Contains(stderr, "stale-launcher-token") {
		t.Fatal(stderr)
	}
}

func TestBranchExportReportsMissingOrInvalidSessionSetup(t *testing.T) {
	fixture := newPushFixture(t)
	config := filepath.Join(fixture.root, "gitconfig")
	saved := filepath.Join(fixture.root, "saved-gitconfig")
	if err := os.Rename(config, saved); err != nil {
		t.Fatal(err)
	}
	output := fixture.push(t, "review/missing")
	stderr := string(output.Stderr)
	if output.Status == 0 || !strings.Contains(stderr, "gitconfig") || !strings.Contains(stderr, worker.SessionSetupGuidance) {
		t.Fatalf("status %d: %s", output.Status, stderr)
	}
	if _, err := os.Stat(config); err == nil {
		t.Fatal("export must not recreate the setup")
	}

	if err := os.Symlink(saved, config); err != nil {
		t.Fatal(err)
	}
	output = fixture.push(t, "review/symlink")
	stderr = string(output.Stderr)
	if output.Status == 0 || !strings.Contains(stderr, "not a regular file") || !strings.Contains(stderr, worker.SessionSetupGuidance) {
		t.Fatalf("status %d: %s", output.Status, stderr)
	}

	if err := os.Remove(config); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(saved, config); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(config, 0o000); err != nil {
		t.Fatal(err)
	}
	// Privileged runners can still read mode-000 files.
	unreadable := false
	if file, err := os.Open(config); err != nil {
		unreadable = true
	} else {
		file.Close()
	}
	output = fixture.push(t, "review/unreadable")
	if err := os.Chmod(config, 0o600); err != nil {
		t.Fatal(err)
	}
	if unreadable {
		stderr = string(output.Stderr)
		if output.Status == 0 || !strings.Contains(stderr, "read ") || !strings.Contains(stderr, "gitconfig") ||
			!strings.Contains(stderr, worker.SessionSetupGuidance) {
			t.Fatalf("status %d: %s", output.Status, stderr)
		}
	}
}

func TestBranchExportPreservesLocalAndExplicitSSHPushesWithoutAToken(t *testing.T) {
	fixture := newPushFixture(t)
	if err := os.Remove(filepath.Join(fixture.repository, ".git", "hooks", "pre-push")); err != nil {
		t.Fatal(err)
	}
	if output := fixture.push(t, "review/local"); output.Status != 0 {
		t.Fatalf("%s", output.Stderr)
	}

	// Exercise Git's SSH transport with a local receive-pack instead of a
	// server.
	ssh := filepath.Join(fixture.directory, "fixture ssh")
	quote := core.PosixQuote
	core.InstallFakeCommand(fixture.directory, "fixture ssh",
		fmt.Sprintf("#!/bin/sh\nexec %s receive-pack %s\n", quote(fixture.git), quote(fixture.remote)))
	configPath := filepath.Join(fixture.root, "launch.json")
	config, err := core.WorkerLaunchConfigRead(configPath)
	if err != nil {
		t.Fatal(err)
	}
	config.TargetEnvironment["GIT_SSH_COMMAND"] = quote(ssh)
	config.TargetEnvironment["GIT_SSH_VARIANT"] = "ssh"
	if err := config.Write(configPath); err != nil {
		t.Fatal(err)
	}
	fixture.runGit(t, fixture.repository, "remote", "set-url", "origin", "ssh://fixture.invalid/session")
	if output := fixture.push(t, "review/ssh"); output.Status != 0 {
		t.Fatalf("%s", output.Stderr)
	}
	for _, branch := range []string{"review/local", "review/ssh"} {
		if !bytes.Equal(
			fixture.runGit(t, fixture.remote, "rev-parse", branch),
			fixture.runGit(t, fixture.repository, "rev-parse", "HEAD"),
		) {
			t.Fatalf("%s is not HEAD", branch)
		}
	}
	if _, err := os.Stat(filepath.Join(fixture.root, "github-token")); err == nil {
		t.Fatal("a token appeared")
	}
}

func TestWorkerGitUsesTheRequestedRepositoryDespiteAPollutedLauncher(t *testing.T) {
	root := t.TempDir()
	executor := core.NewBoundedProcessExecutor(15 * time.Second)
	git := func(args ...string) {
		command := core.NewCommandSpec("git", args)
		command.Cwd = &root
		command.ClearEnv = true
		command.Env = map[string]string{
			"PATH":                "/usr/bin:/bin",
			"HOME":                root,
			"GIT_CONFIG_NOSYSTEM": "1",
			"GIT_CONFIG_GLOBAL":   "/dev/null",
		}
		output, err := executor.Execute(&command)
		if err != nil {
			t.Fatal(err)
		}
		if output.Status != 0 {
			t.Fatalf("git %v: %s", args, output.Stderr)
		}
	}
	git("init", "-q")
	if err := os.WriteFile(filepath.Join(root, "file"), []byte("before\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	git("add", "file")
	git("-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "initial")
	if err := os.WriteFile(filepath.Join(root, "file"), []byte("after\n"), 0o666); err != nil {
		t.Fatal(err)
	}

	command := core.NewCommandSpec(workerExecutable, []string{"worker", "diff", "--repository", root, "--base", "HEAD"})
	command.Env = map[string]string{
		"HOME":             filepath.Join(root, "wrong-home"),
		"SHELL":            "/bin/false",
		"PATH":             "/nonexistent-parent-path",
		"GIT_DIR":          "/nonexistent-parent-repository",
		"GIT_WORK_TREE":    "/nonexistent-parent-worktree",
		"CARGO_TARGET_DIR": "/parent/build",
		"RUSTFLAGS":        "parent flags",
	}
	output, err := executor.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%s", output.Stderr)
	}
	if !strings.Contains(string(output.Stdout), "-before\n+after") {
		t.Fatalf("expected repository diff: %s", output.Stdout)
	}
}

// stageWorkers links the built worker as root's `hel` and as this build's
// lifecycle control worker.
func stageWorkers(t *testing.T, root string) {
	t.Helper()
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(workerExecutable, filepath.Join(root, "hel")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(workerExecutable, targets.ControlWorkerPath(root)); err != nil {
		t.Fatal(err)
	}
}

// runControlScript runs a lifecycle control script under executor.
func runControlScript(executor core.BoundedProcessExecutor, script targets.ControlScript) (core.CommandOutput, error) {
	command := core.NewCommandSpec("sh", []string{"-c", script.UnstagedScript()})
	return executor.Execute(&command)
}

func TestCheckpointWorkerRemainsVisibleAndStoppableAfterCleanReexec(t *testing.T) {
	root := t.TempDir()
	workerRoot := filepath.Join(root, "worker")
	if _, err := worker.DurableRelayOpen(workerRoot, stopTestSession, "test"); err != nil {
		t.Fatal(err)
	}
	stageWorkers(t, workerRoot)
	config := filepath.Join(root, "launch.json")
	writeJSON(t, config, map[string]any{
		"run_mode": "checkpoint_only", "session_id": stopTestSession, "harness": "codex",
		"bridge_command": "/missing-harness", "bridge_args": []string{},
		"environment":        map[string]any{"CODEX_HOME": filepath.Join(root, "profile")},
		"target_environment": map[string]any{"MJ_INSTANCE": "qa-empty-recovery-1063"},
		"cwd":                root, "execution_policy": "configured_approvals",
	})
	command := core.NewCommandSpec(filepath.Join(workerRoot, "hel"), []string{
		"worker", "run", "--root", workerRoot, "--config", config,
	})
	command.Env = map[string]string{
		"SHELL":       "/missing-parent-shell",
		"MJ_INSTANCE": "wrong-launcher-instance",
	}
	running := executeInBackground(command, 15*time.Second)
	executor := core.NewBoundedProcessExecutor(8 * time.Second)
	observation := func() error {
		deadline := time.Now().Add(5 * time.Second)
		for {
			if _, err := os.Stat(filepath.Join(workerRoot, "control.sock")); err == nil {
				break
			}
			if !time.Now().Before(deadline) {
				return errors.New("checkpoint worker did not start")
			}
			time.Sleep(25 * time.Millisecond)
		}
		output, err := runControlScript(executor, targets.WorkerDaemonLivenessScript(workerRoot))
		if err != nil {
			return err
		}
		if output.Status != 0 || string(output.Stdout) != "alive\n" {
			return fmt.Errorf("re-executed worker lost its lifecycle identity: %d %q %s", output.Status, output.Stdout, output.Stderr)
		}
		if runtime.GOOS == "linux" {
			pid, err := os.ReadFile(filepath.Join(workerRoot, core.WorkerPIDFile))
			if err != nil {
				return err
			}
			environment, err := os.ReadFile("/proc/" + strings.TrimSpace(string(pid)) + "/environ")
			if err != nil {
				return err
			}
			// Neither the fingerprint an older daemon saved in launch.json
			// nor the launcher's own selection may reach the worker: every
			// `mj` a harness runs would read it as a named instance.
			for _, entry := range bytes.Split(environment, []byte{0}) {
				if bytes.HasPrefix(entry, []byte("MJ_INSTANCE=")) {
					return errors.New("worker environment selects a Mjolnir instance")
				}
			}
		}
		return nil
	}()
	// Always stop/join before dropping the worker's files, even if
	// observation failed. The bounded owner also cleans up if the identity
	// check regresses.
	stopped, stopErr := runControlScript(executor, targets.StopWorkerDaemonScript(workerRoot))
	result := <-running
	if observation != nil {
		t.Fatal(observation)
	}
	if stopErr != nil || stopped.Status != 0 {
		t.Fatalf("stop: %v %v", stopErr, stopped)
	}
	if result.err != nil {
		t.Fatalf("lifecycle stop did not terminate the worker before its deadline: %v", result.err)
	}
}

// containerWorkerTokenAfterStart starts a container worker whose launcher
// environment carries the GitHub token the controller put in the
// container, as `podman exec` does, and returns the session's token once
// the worker has passed its GitHub setup.
func containerWorkerTokenAfterStart(t *testing.T, existing *string, noGithubCredentials bool) *string {
	t.Helper()
	root := t.TempDir()
	workerRoot := filepath.Join(root, "worker")
	if err := os.MkdirAll(workerRoot, 0o777); err != nil {
		t.Fatal(err)
	}
	tokenPath := filepath.Join(workerRoot, "github-token")
	if existing != nil {
		if _, err := core.WriteGithubToken(tokenPath, []byte(*existing)); err != nil {
			t.Fatal(err)
		}
	}
	config := filepath.Join(workerRoot, "launch.json")
	writeJSON(t, config, map[string]any{
		"session_id": stopTestSession, "harness": "codex",
		"seed_image_environment": true,
		"no_github_credentials":  noGithubCredentials,
		"bridge_command":         "/missing-harness", "bridge_args": []string{},
		"environment":        map[string]any{"CODEX_HOME": filepath.Join(root, "profile")},
		"target_environment": map[string]any{"MJ_INSTANCE": "github-credentials-1474", "GITHUB_TOKEN": "target-token"},
		"cwd":                root,
		"execution_policy":   "configured_approvals",
	})
	stageWorkers(t, workerRoot)
	command := core.NewCommandSpec(workerExecutable, []string{
		"worker", "run", "--root", workerRoot, "--config", config,
	})
	command.Env = map[string]string{"GH_TOKEN": "container-token"}
	running := executeInBackground(command, 15*time.Second)
	// The harness step comes after the GitHub setup.
	startup := filepath.Join(workerRoot, core.WorkerStartupFile)
	reachedHarness := func() bool {
		steps, err := os.ReadFile(startup)
		return err == nil && strings.Contains(string(steps), "harness-resolve")
	}
	var result executed
	received := false
	deadline := time.Now().Add(10 * time.Second)
	for !finished(running, &result, &received) && time.Now().Before(deadline) && !reachedHarness() {
		time.Sleep(25 * time.Millisecond)
	}
	prepared := reachedHarness()
	_, _ = runControlScript(core.NewBoundedProcessExecutor(8*time.Second), targets.StopWorkerDaemonScript(workerRoot))
	if !received {
		result = <-running
	}
	if result.err == nil {
		for _, stream := range [][]byte{result.output.Stdout, result.output.Stderr} {
			if strings.Contains(string(stream), "container-token") {
				t.Fatal("the container token leaked into the worker's output")
			}
		}
	}
	if !prepared {
		t.Fatal("worker must reach its GitHub setup before checking credentials")
	}
	_, token, err := core.ReadGithubToken(tokenPath)
	if err != nil {
		t.Fatal(err)
	}
	return token
}

func tokenText(token *string) string {
	if token == nil {
		return "<none>"
	}
	return *token
}

// Hard-won: fedacb5: the first container turn ran without the synchronized GitHub token after re-exec.
func TestContainerWorkerHasTheContainerGithubTokenWhenItStarts(t *testing.T) {
	// The worker re-executes itself with a cleared environment before it
	// sets up the session, so the token has to be kept from the launcher.
	// Without it the first turn ran with no GitHub credentials until the
	// controller's periodic sync caught up (#1194).
	if token := containerWorkerTokenAfterStart(t, nil, false); tokenText(token) != "container-token" {
		t.Fatalf("token %s", tokenText(token))
	}
}

func TestContainerWorkerRestartKeepsANewerSyncedGithubToken(t *testing.T) {
	// The container's variable is fixed when the container is created. The
	// credential sync owns every later change, so a restart must not undo
	// it.
	synced := "synced-token"
	if token := containerWorkerTokenAfterStart(t, &synced, false); tokenText(token) != "synced-token" {
		t.Fatalf("token %s", tokenText(token))
	}
}

func TestCredentialFreeContainerWorkerNeverSeedsOrReusesAGithubToken(t *testing.T) {
	if token := containerWorkerTokenAfterStart(t, nil, true); token != nil {
		t.Fatalf("token %s", *token)
	}
	synced := "synced-token"
	if token := containerWorkerTokenAfterStart(t, &synced, true); token != nil {
		t.Fatalf("token %s", *token)
	}
}
