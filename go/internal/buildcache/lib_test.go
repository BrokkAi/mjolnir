// Port of mj-build-cache/src/lib.rs's test setup, and the hand-written
// executors the crate's tests share.

package buildcache

import (
	"io"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Rust: `#[cfg(test)] mj_core::enable_test_fixtures_on_startup!();`, and
// `cfg!(test)` for this package's own tests.
func init() {
	cfgTest = true
	core.EnableTestFixtures()
}

// funcExecutor is a hand-written CommandExecutor whose behaviour is a set of
// functions, standing in for the Rust tests' one-off executor types. Unset
// functions keep the Rust trait's defaults.
type funcExecutor struct {
	core.CommandExecutorDefaults
	execute            func(command *core.CommandSpec) (core.CommandOutput, error)
	notice             func(notice string)
	cancelled          func() bool
	cleanupWithTimeout func(command *core.CommandSpec, timeout time.Duration) (core.CommandOutput, error)
}

func (e *funcExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.execute(command)
}

func (e *funcExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *funcExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, timeout time.Duration) (core.CommandOutput, error) {
	if e.cleanupWithTimeout != nil {
		return e.cleanupWithTimeout(command, timeout)
	}
	return e.ExecuteCleanup(command)
}

func (e *funcExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *funcExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e *funcExecutor) CancellationRequested() bool {
	if e.cancelled != nil {
		return e.cancelled()
	}
	return false
}

func (e *funcExecutor) CancellationRequestedWithoutDeadline() bool { return e.CancellationRequested() }

func (e *funcExecutor) NotifyNotice(notice string) {
	if e.notice != nil {
		e.notice(notice)
	}
}

func (e *funcExecutor) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

// processWithEnv runs commands as local processes with extra environment
// variables, as the Rust tests' executors that set PATH or HOME before
// delegating to ProcessExecutor do.
func processWithEnv(environment map[string]string) *funcExecutor {
	return &funcExecutor{execute: func(command *core.CommandSpec) (core.CommandOutput, error) {
		copied := *command
		copied.Env = maps.Clone(command.Env)
		if copied.Env == nil {
			copied.Env = map[string]string{}
		}
		maps.Copy(copied.Env, environment)
		return core.ProcessExecutor{}.Execute(&copied)
	}}
}

// output is a finished command's output from text.
func output(status int, stdout, stderr string) core.CommandOutput {
	return core.CommandOutput{Status: status, Stdout: []byte(stdout), Stderr: []byte(stderr)}
}

func stringPointer(value string) *string { return &value }

func boolPointer(value bool) *bool { return &value }

// skipOnWindows is Rust's `#[cfg_attr(windows, ignore = ...)]`.
func skipOnWindows(t *testing.T, reason string) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip(reason)
	}
}

// requireUnix is Rust's `#[cfg(unix)]` on a test.
func requireUnix(t *testing.T) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
}

// requireLinux is Rust's `#[cfg(target_os = "linux")]` on a test.
func requireLinux(t *testing.T) {
	t.Helper()
	if runtime.GOOS != "linux" {
		t.Skip("Linux only")
	}
}

// lookPath is the first executable named name on PATH, as the Rust tests
// find a utility to link or wrap.
func lookPath(t *testing.T, name string) string {
	t.Helper()
	path, err := exec.LookPath(name)
	if err != nil {
		t.Fatalf("could not find test utility %s: %v", name, err)
	}
	return path
}

func writeExecutable(t *testing.T, path, contents string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(contents), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

func mustRun(t *testing.T, command core.CommandSpec) core.CommandOutput {
	t.Helper()
	result, err := core.ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	return result
}

func lossyTrim(data []byte) string { return strings.TrimSpace(lossy(data)) }

func readText(t *testing.T, path string) string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

// optionalText renders an optional string for a failure message.
func optionalText(value *string) string {
	if value == nil {
		return "<none>"
	}
	return *value
}

// exists is Rust's `Path::exists`.
func exists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// canonical is Rust's `std::fs::canonicalize`.
func canonical(t *testing.T, path string) string {
	t.Helper()
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		t.Fatal(err)
	}
	absolute, err := filepath.Abs(resolved)
	if err != nil {
		t.Fatal(err)
	}
	return absolute
}

func mkdirAll(t *testing.T, paths ...string) {
	t.Helper()
	for _, path := range paths {
		if err := os.MkdirAll(path, 0o777); err != nil {
			t.Fatal(err)
		}
	}
}

func symlink(t *testing.T, target, link string) {
	t.Helper()
	if err := os.Symlink(target, link); err != nil {
		t.Fatal(err)
	}
}

// fakeMbxScript is a stand-in mbx that answers `--version` with version.
func fakeMbxScript(version string) string {
	return "#!/bin/sh\nprintf 'mbx " + version + "\\n'\n"
}
