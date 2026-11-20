// Port of the `cfg(unix)` tests of mj-worker/src/user_shell.rs.

//go:build unix

package worker

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestUserShellRestoresGithubWrapperAfterLoginProfile(t *testing.T) {
	cwd := t.TempDir()
	worker := t.TempDir()
	bin := filepath.Join(worker, "bin")
	if err := os.Mkdir(bin, 0o755); err != nil {
		t.Fatal(err)
	}
	wrapper := filepath.Join(bin, "gh")
	core.InstallFakeCommand(bin, "gh", "#!/bin/sh\nexit 0\n")
	environment := map[string]string{
		"PATH":          "/usr/bin:/bin",
		"GH_TOKEN":      "stale-token",
		"GITHUB_TOKEN":  "also-stale",
		githubCLIBinEnv: bin,
	}
	events := make(chan core.RuntimeEvent, 16)
	shells := NewUserShellRegistry(cwd, environment, events)
	defer shells.Close()
	if err := shells.Start(
		"shell-github-path",
		`printf '%s\n%s|%s\n' "$(command -v gh)" "${GH_TOKEN-unset}" "${GITHUB_TOKEN-unset}"`,
	); err != nil {
		t.Fatal(err)
	}

	result := userShellFinished(t, events, 5*time.Second, "user shell did not finish")
	if result.Status != core.UserShellStatusExited {
		t.Fatalf("status = %s: %+v", result.Status, result)
	}
	if want := fmt.Sprintf("%s\nunset|unset\n", wrapper); result.Stdout != want {
		t.Fatalf("stdout = %q, want %q", result.Stdout, want)
	}
}

// Hard-won: 6c391e1b: a target PATH without bash made user shell commands fail.
func TestUserShellRunsCommandTextThroughShWhenPathLacksBash(t *testing.T) {
	cwd := t.TempDir()
	bin := t.TempDir()
	if err := os.Symlink("/bin/sh", filepath.Join(bin, "sh")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("/bin/ls", filepath.Join(bin, "ls")); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(cwd, "marker-file"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	events := make(chan core.RuntimeEvent, 16)
	shells := NewUserShellRegistry(cwd, map[string]string{"PATH": bin}, events)
	defer shells.Close()
	if err := shells.Start("shell-ls", "ls -a"); err != nil {
		t.Fatal(err)
	}
	result := userShellFinished(t, events, 5*time.Second, "user shell did not finish")
	if result.Status != core.UserShellStatusExited {
		t.Fatalf("%+v", result)
	}
	if !strings.Contains(result.Stdout, "marker-file") {
		t.Fatalf("%+v", result)
	}
}

func TestCancellationStillOwnsPipesAfterTheShellLeaderExits(t *testing.T) {
	cwd := t.TempDir()
	events := make(chan core.RuntimeEvent, 64)
	shells := NewUserShellRegistry(cwd, map[string]string{}, events)
	defer shells.Close()
	if err := shells.Start(
		"inherited-pipes",
		"echo $$ > leader; head -c 131072 /dev/zero; (trap '' TERM; sleep 60) & exit 0",
	); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		if text, err := os.ReadFile(filepath.Join(cwd, "leader")); err == nil {
			// Signal zero only checks this fixture's leader.
			if pid, err := strconv.Atoi(strings.TrimSpace(string(text))); err == nil && syscall.Kill(pid, 0) != nil {
				break
			}
		}
		if time.Now().After(deadline) {
			t.Fatal("the shell leader did not exit")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if outcome := shells.Cancel("inherited-pipes"); outcome != UserShellCancelOutcomeRequested {
		t.Fatalf("cancel = %s", outcome)
	}
	result := userShellFinished(t, events, 5*time.Second, "user shell did not finish")
	if result.Status != core.UserShellStatusCancelled {
		t.Fatalf("status = %s", result.Status)
	}
	if !result.StdoutTruncated {
		t.Fatal("expected truncated stdout")
	}
}

func TestCoordinatorDropRetainsShellCleanupUntilProcessesStop(t *testing.T) {
	cwd := t.TempDir()
	events := make(chan core.RuntimeEvent, 1)
	shells := NewUserShellRegistry(cwd, map[string]string{}, events)
	completion := shells.completionTracker()
	if err := shells.Start("coordinator-drop", "sleep 60 & wait"); err != nil {
		t.Fatal(err)
	}
	// Rust drops the registry and the event receiver here.
	shells.Close()
	stopped := make(chan struct{})
	go func() {
		completion.Wait()
		close(stopped)
	}()
	select {
	case <-stopped:
	case <-time.After(5 * time.Second):
		t.Fatal("coordinator must retain cleanup after its registry is dropped")
	}
}
