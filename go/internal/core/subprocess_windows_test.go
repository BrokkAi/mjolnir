//go:build windows

// Port of the Windows test in mj-core/src/subprocess.rs.

package core

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// Hard-won: #1235: A detached Windows daemon kept its launcher's stdout pipe, so capturing `mj daemon restart` output hung until the daemon exited.
func TestSpawnDetachedChildKeepsNoStandardHandleOfItsLauncher(t *testing.T) {
	// This test binary is its own launcher: run with this variable, the test
	// detaches a long-lived child and exits, as `mj daemon restart` does,
	// while its stdout is a pipe the outer test reads.
	const launcherLog = "MJ_TEST_DETACHED_LAUNCHER_LOG"
	if log := os.Getenv(launcherLog); log != "" {
		pid, err := SpawnDetached(exec.Command("ping", "-n", "60", "127.0.0.1"), log)
		if err != nil {
			t.Fatalf("detach the long-lived child: %v", err)
		}
		fmt.Printf("detached=%d\n", pid)
		return
	}
	launcher := exec.Command(os.Args[0], "-test.run", "^TestSpawnDetachedChildKeepsNoStandardHandleOfItsLauncher$", "-test.v")
	launcher.Env = append(os.Environ(), launcherLog+"="+filepath.Join(t.TempDir(), "child.log"))
	stdout, err := launcher.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := launcher.Start(); err != nil {
		t.Fatalf("start the launcher: %v", err)
	}
	outcome := make(chan string, 1)
	failed := make(chan error, 1)
	go func() {
		output, err := io.ReadAll(stdout)
		if err != nil {
			failed <- err
			return
		}
		outcome <- string(output)
	}()
	// The detached child pings for about a minute; EOF must come first.
	var output string
	select {
	case output = <-outcome:
	case err := <-failed:
		t.Fatalf("read the launcher's output: %v", err)
	case <-time.After(30 * time.Second):
		t.Fatal("the pipe must reach EOF while the detached child is still running")
	}
	_ = launcher.Wait()
	var pid string
	for _, line := range strings.Split(output, "\n") {
		if rest, ok := strings.CutPrefix(strings.TrimSpace(line), "detached="); ok {
			pid = rest
		}
	}
	if pid == "" {
		t.Fatalf("the launcher reports its child: %s", output)
	}
	if _, err := RunWithInput(exec.Command("taskkill", "/PID", pid, "/F"), nil); err != nil {
		t.Fatalf("stop the detached child: %v", err)
	}
}
