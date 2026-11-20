// Port of the `cfg(target_os = "linux")` tests of mj-worker/src/user_shell.rs.

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

// processIsGone reports a process that no longer exists or is a zombie.
func processIsGone(pid int) bool {
	state, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return true
	}
	end := strings.LastIndex(string(state), ") ")
	return end >= 0 && strings.HasPrefix(string(state)[end+2:], "Z")
}

// B-2: a backgrounded descendant holds stdout and stderr open.
// Hard-won: c86b2123: descendants holding pipes kept completed commands running.
func TestUserShellCompletesAtLeaderExitAndStopsADescendantHoldingThePipes(t *testing.T) {
	cwd := t.TempDir()
	events := make(chan core.RuntimeEvent, 64)
	shells := NewUserShellRegistry(cwd, map[string]string{}, events)
	defer shells.Close()
	if err := shells.Start(
		"held-pipes",
		"(sleep 0.5; echo during-drain; sleep 300; echo too-late) & echo $! > descendant; echo hi; exit 3",
	); err != nil {
		t.Fatal(err)
	}
	result := userShellFinished(t, events, 5*time.Second, "user shell did not finish")
	if result.Status != core.UserShellStatusExited {
		t.Fatalf("%+v", result)
	}
	if result.ExitCode == nil || *result.ExitCode != 3 {
		t.Fatalf("%+v", result)
	}
	if result.Stdout != "hi\nduring-drain\n" {
		t.Fatalf("stdout = %q", result.Stdout)
	}
	// The duration is the leader's, not the drain's.
	if result.DurationMs >= 1500 {
		t.Fatalf("%+v", result)
	}
	text, err := os.ReadFile(filepath.Join(cwd, "descendant"))
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(text)))
	if err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(2 * time.Second)
	for !processIsGone(pid) {
		if !time.Now().Before(deadline) {
			// Cleans up this test's own fixture.
			syscall.Kill(pid, syscall.SIGKILL)
			t.Fatalf("descendant %d survived the completed command", pid)
		}
		time.Sleep(10 * time.Millisecond)
	}
	if slots := shells.AvailableSlots(); slots != MaxConcurrentUserShells-1 {
		t.Fatalf("slots = %d", slots)
	}
	shells.Completed("held-pipes")
	if slots := shells.AvailableSlots(); slots != MaxConcurrentUserShells {
		t.Fatalf("slots = %d", slots)
	}
}
