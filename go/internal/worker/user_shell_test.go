// Port of the tests of mj-worker/src/user_shell.rs that every platform
// builds.

package worker

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// userShellFinished waits up to timeout for the next shell result on
// received.
func userShellFinished(t *testing.T, received <-chan core.RuntimeEvent, timeout time.Duration, failure string) core.UserShellResult {
	t.Helper()
	deadline := time.After(timeout)
	for {
		select {
		case event := <-received:
			if finished, ok := event.(core.RuntimeEventUserShellFinished); ok {
				return finished.Result
			}
		case <-deadline:
			t.Fatal(failure)
		}
	}
}

// Hard-won: 6c391e1b: ENOENT diagnostics named the command instead of the missing shell.
func TestMissingLoginShellIsNamedInsteadOfTheCommand(t *testing.T) {
	_, err := userShellProgram(map[string]string{"SHELL": "/nonexistent/bin/bash"})
	if err == nil || err.Error() != "shell /nonexistent/bin/bash not found" {
		t.Fatalf("error = %v", err)
	}
	shell, err := userShellProgram(map[string]string{"SHELL": "/usr/bin/fish"})
	if err != nil || shell != "/bin/sh" {
		t.Fatalf("shell = %q, %v", shell, err)
	}
}

func TestOutputKeepsHeadAndTailWithoutUnboundedGrowth(t *testing.T) {
	var output headTailBuffer
	output.append([]byte(strings.Repeat("a", userShellOutputHeadBytes)))
	output.append([]byte(strings.Repeat("b", 70*1024)))
	text, truncated := output.finalText()
	if !truncated {
		t.Fatal("expected truncated")
	}
	if !strings.HasPrefix(text, strings.Repeat("a", userShellOutputHeadBytes)) {
		t.Fatal("head missing")
	}
	if !strings.HasSuffix(text, strings.Repeat("b", userShellOutputTailBytes)) {
		t.Fatal("tail missing")
	}
	if !strings.Contains(text, "mj dropped") {
		t.Fatal("drop marker missing")
	}
	if len(output.head) != userShellOutputHeadBytes || len(output.tail) != userShellOutputTailBytes {
		t.Fatalf("head %d tail %d", len(output.head), len(output.tail))
	}
}

func TestUserShellDrainsMoreThanAPipeBufferFromBothStreams(t *testing.T) {
	cwd := t.TempDir()
	events := make(chan core.RuntimeEvent, 64)
	shells := NewUserShellRegistry(cwd, map[string]string{}, events)
	defer shells.Close()
	if err := shells.Start(
		"shell-large-output",
		`awk 'BEGIN { for (i = 0; i < 70000; i++) printf "x" }'; printf '\nCWD_MARKER\n'; : > user-shell-cwd-marker; awk 'BEGIN { for (i = 0; i < 70000; i++) printf "y" }' >&2`,
	); err != nil {
		t.Fatal(err)
	}

	result := userShellFinished(t, events, 10*time.Second, "large-output shell deadlocked")
	if result.Status != core.UserShellStatusExited {
		t.Fatalf("status = %s", result.Status)
	}
	if result.ExitCode == nil || *result.ExitCode != 0 {
		t.Fatalf("exit code = %v", result.ExitCode)
	}
	if !result.StdoutTruncated || !result.StderrTruncated {
		t.Fatalf("%+v", result)
	}
	if !strings.Contains(result.Stdout, "CWD_MARKER") {
		t.Fatal("stdout misses the marker")
	}
	if info, err := os.Stat(filepath.Join(cwd, "user-shell-cwd-marker")); err != nil || !info.Mode().IsRegular() {
		t.Fatal("the shell did not run in its working directory")
	}
	if !strings.HasPrefix(result.Stdout, strings.Repeat("x", userShellOutputHeadBytes)) ||
		!strings.HasPrefix(result.Stderr, strings.Repeat("y", userShellOutputHeadBytes)) {
		t.Fatal("the heads of both streams must be kept")
	}
}
