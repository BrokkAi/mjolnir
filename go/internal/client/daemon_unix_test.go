// Port of the Unix tests in mj-client/src/daemon.rs.

//go:build unix

package client

import (
	"os/exec"
	"testing"
	"time"
)

// Hard-won: c672eb41a1bf: macOS startup hang waited forever for an exited but unreaped daemon
func TestAnExitedUnreapedProcessIsAZombieAndNotAlive(t *testing.T) {
	// An upgrade waits for the old daemon to leave. If that daemon exited
	// but is still a child nobody has reaped, `kill(pid, 0)` keeps finding
	// it, so only the zombie check lets the wait end.
	running := exec.Command("sleep", "30")
	if err := running.Start(); err != nil {
		t.Fatal(err)
	}
	runningPid := uint32(running.Process.Pid)
	if !ProcessIsAlive(runningPid) {
		t.Fatal("a running child is not alive")
	}
	if ProcessIsZombie(runningPid) {
		t.Fatal("a running child is a zombie")
	}
	if err := running.Process.Kill(); err != nil {
		t.Fatal(err)
	}
	_ = running.Wait()

	exited := exec.Command("sh", "-c", "exit 0")
	if err := exited.Start(); err != nil {
		t.Fatal(err)
	}
	pid := uint32(exited.Process.Pid)
	deadline := time.Now().Add(10 * time.Second)
	for !ProcessIsZombie(pid) {
		if !time.Now().Before(deadline) {
			t.Fatal("an exited, unreaped child was never reported as a zombie")
		}
		time.Sleep(20 * time.Millisecond)
	}
	if ProcessIsAlive(pid) {
		t.Fatal("a zombie is alive")
	}
	if err := exited.Wait(); err != nil {
		t.Fatal(err)
	}
}
