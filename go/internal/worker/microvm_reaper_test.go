//go:build linux

// Port of mj-worker/tests/microvm_reaper.rs.
//
// The guest agent's PID 1 reaper owns every `waitpid`. Rust runs it alone in
// a test binary of its own because it collects the exit status of every
// child of the process; here the test re-runs the test binary for just this
// test, so the reaper never takes another test's children.

package worker

import (
	"errors"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const microvmReaperChildEnvironment = "MJ_TEST_MICROVM_REAPER_ALONE"

// Hard-won: a spawn whose exec fails (a missing working directory) panicked in
// std when the reaper had already collected the half-started child, and the
// panic poisoned the reaper's lock for every later exec in the VM.
func TestReaperSurvivesFailedSpawnsAndReportsExitsAndOrphans(t *testing.T) {
	if os.Getenv(microvmReaperChildEnvironment) != "1" {
		command := exec.Command(os.Args[0], "-test.run=^TestReaperSurvivesFailedSpawnsAndReportsExitsAndOrphans$", "-test.count=1", "-test.v")
		command.Env = append(os.Environ(), microvmReaperChildEnvironment+"=1")
		output, err := core.RunWithInput(command, nil)
		if err != nil {
			t.Fatal(err)
		}
		if !output.Status.Success() {
			t.Fatalf("the reaper test process failed (%s):\n%s%s", microvmExitStatus(output.Status), output.Stdout, output.Stderr)
		}
		if !strings.Contains(string(output.Stdout), "--- PASS: TestReaperSurvivesFailedSpawnsAndReportsExitsAndOrphans") {
			t.Fatalf("the reaper test did not run:\n%s", output.Stdout)
		}
		return
	}

	// Orphans reparent to this process, as they do to PID 1.
	if err := unix.Prctl(unix.PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0); err != nil {
		t.Fatal(err)
	}
	reaper, err := MicrovmStartReaper()
	if err != nil {
		t.Fatal(err)
	}

	for range 50 {
		missingDirectory := exec.Command("true")
		missingDirectory.Dir = "/nonexistent-directory"
		// Without process attributes Go checks the directory before it
		// forks; with them, as every agent spawn has, the child's chdir
		// fails after the fork, which is the case this test is about.
		missingDirectory.SysProcAttr = &syscall.SysProcAttr{}
		_, err := reaper.Spawn(missingDirectory)
		if err == nil {
			t.Fatal("spawn must fail")
		}
		if !errors.Is(err, fs.ErrNotExist) {
			t.Fatalf("%v", err)
		}
	}

	spawned, err := reaper.Spawn(exec.Command("sh", "-c", "exit 3"))
	if err != nil {
		t.Fatal(err)
	}
	select {
	case result := <-spawned.Exit:
		if result.Err != nil || !result.Status.Exited() || result.Status.ExitStatus() != 3 {
			t.Fatalf("%+v", result)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("no exit status within 10 seconds")
	}

	// A grandchild outlives its parent and is reaped without anyone waiting
	// for it.
	pidRead, pidWrite, err := microvmPipe()
	if err != nil {
		t.Fatal(err)
	}
	orphans := exec.Command("sh", "-c", "sleep 0.2 & echo $!")
	orphans.Stdout = pidWrite
	_, err = reaper.Spawn(orphans)
	pidWrite.Close()
	if err != nil {
		t.Fatal(err)
	}
	pidText, err := io.ReadAll(pidRead)
	pidRead.Close()
	if err != nil {
		t.Fatal(err)
	}
	orphan, err := strconv.ParseUint(strings.TrimSpace(string(pidText)), 10, 32)
	if err != nil {
		t.Fatal(err)
	}
	for range 100 {
		if _, err := os.Stat("/proc/" + strconv.FormatUint(orphan, 10)); err != nil {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("orphan %d was never reaped", orphan)
}
