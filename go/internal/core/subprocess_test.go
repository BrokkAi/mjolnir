//go:build unix

// Port of the Unix tests in mj-core/src/subprocess.rs.

package core

import (
	"bytes"
	"context"
	"errors"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

// subprocessReadPID polls file until it holds a process ID.
func subprocessReadPID(t *testing.T, file string) int {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for {
		if body, err := os.ReadFile(file); err == nil {
			if pid, err := strconv.Atoi(strings.TrimSpace(string(body))); err == nil {
				return pid
			}
		}
		if time.Now().After(deadline) {
			t.Fatalf("no pid in %s", file)
		}
		time.Sleep(time.Millisecond)
	}
}

// subprocessWaitGone polls until pid no longer exists.
func subprocessWaitGone(t *testing.T, pid int) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for syscall.Kill(pid, 0) == nil {
		if time.Now().After(deadline) {
			t.Fatalf("process %d survived", pid)
		}
		time.Sleep(time.Millisecond)
	}
}

func TestCaptureDrainsBothPipesBeyondPipeCapacity(t *testing.T) {
	output, err := Capture(context.Background(), exec.Command("python3", "-c",
		"import os; os.write(1, b'o'*262144); os.write(2, b'e'*262144)"))
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("status %v", output.Status)
	}
	if !bytes.Equal(output.Stdout, bytes.Repeat([]byte{'o'}, 262144)) {
		t.Fatalf("stdout %d bytes", len(output.Stdout))
	}
	if !bytes.Equal(output.Stderr, bytes.Repeat([]byte{'e'}, 262144)) {
		t.Fatalf("stderr %d bytes", len(output.Stderr))
	}
}

func TestSupervisedChildDrainsLargeStderrAndReapsOnCancel(t *testing.T) {
	child, err := SupervisedChildSpawn(exec.Command("sh", "-c", "head -c 131072 /dev/zero >&2; sleep 60 & wait"))
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	for len(child.Stderr()) < 131072 {
		if ctx.Err() != nil {
			t.Fatal("stderr was not drained")
		}
		runtime.Gosched()
	}
	status, err := child.Terminate(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if status.Success() {
		t.Fatal("a terminated child does not succeed")
	}
	if len(child.Stderr()) != 131072 {
		t.Fatalf("stderr %d bytes", len(child.Stderr()))
	}
}

func TestLoggedCommandCancellationKillsItsProcessGroup(t *testing.T) {
	temp := t.TempDir()
	pidFile := filepath.Join(temp, "pid")
	logPath := filepath.Join(temp, "log")
	output, err := os.Create(logPath)
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	ctx, cancel := context.WithCancel(context.Background())
	result := make(chan error, 1)
	go func() {
		_, err := RunToFileAsync(ctx, exec.Command("sh", "-c",
			"head -c 131072 /dev/zero; echo diagnostics >&2; echo $$ > \"$1\"; sleep 60 & wait", "logged-fixture", pidFile), output)
		result <- err
	}()
	pid := subprocessReadPID(t, pidFile)
	cancel()
	if err := <-result; !errors.Is(err, context.Canceled) {
		t.Fatalf("the command was not cancelled: %v", err)
	}
	subprocessWaitGone(t, pid)
	log, err := os.ReadFile(logPath)
	if err != nil {
		t.Fatal(err)
	}
	if len(log) <= 131072 || !bytes.HasSuffix(log, []byte("diagnostics\n")) {
		t.Fatalf("log of %d bytes ends %q", len(log), log[max(len(log)-20, 0):])
	}
}

func TestSupervisedChildDropReapsProcessGroup(t *testing.T) {
	pidFile := filepath.Join(t.TempDir(), "pid")
	child, err := SupervisedChildSpawn(exec.Command("sh", "-c", "echo $$ > \"$1\"; sleep 60 & wait", "fixture", pidFile))
	if err != nil {
		t.Fatal(err)
	}
	pid := subprocessReadPID(t, pidFile)
	child.Close()
	subprocessWaitGone(t, pid)
}

func TestBoundedTimeoutReapsTheChildBeforeReturning(t *testing.T) {
	pidFile := filepath.Join(t.TempDir(), "child.pid")
	_, err := RunBounded(context.Background(),
		exec.Command("sh", "-c", "echo $$ > \"$1\"; sleep 60 & wait", "reap-fixture", pidFile),
		100_000, 200*time.Millisecond)
	if err == nil || !strings.Contains(err.Error(), "timed out") {
		t.Fatalf("error %v", err)
	}
	body, readErr := os.ReadFile(pidFile)
	if readErr != nil {
		t.Fatal(readErr)
	}
	pid, parseErr := strconv.Atoi(strings.TrimSpace(string(body)))
	if parseErr != nil {
		t.Fatal(parseErr)
	}
	var status syscall.WaitStatus
	waited, waitErr := syscall.Wait4(pid, &status, syscall.WNOHANG, nil)
	if waited != -1 || !errors.Is(waitErr, syscall.ECHILD) {
		t.Fatalf("bounded subprocess must already be reaped: %d %v", waited, waitErr)
	}
}

func TestBoundedCaptureDrainsLargePipesAndStopsOnOverflowOrTimeout(t *testing.T) {
	output, err := RunBounded(context.Background(),
		exec.Command("sh", "-c", "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2"), 200_000, 10*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if len(output.Stdout) != 131072 || len(output.Stderr) != 131072 {
		t.Fatalf("stdout %d stderr %d", len(output.Stdout), len(output.Stderr))
	}
	_, err = RunBounded(context.Background(),
		exec.Command("sh", "-c", "head -c 200000 /dev/zero; sleep 30"), 100_000, 10*time.Second)
	if err == nil || !strings.Contains(err.Error(), "exceeds") {
		t.Fatalf("overflow %v", err)
	}
	_, err = RunBounded(context.Background(), exec.Command("sh", "-c", "sleep 30 & wait"), 100_000, 100*time.Millisecond)
	if err == nil || !strings.Contains(err.Error(), "timed out") {
		t.Fatalf("timeout %v", err)
	}
}

func TestBoundedInputStreamsMoreThanOnePipeBufferConcurrently(t *testing.T) {
	input := bytes.Repeat([]byte{'x'}, 512*1024)
	output, err := RunBoundedWithInput(context.Background(), exec.Command("sh", "-c", "cat"), input, 600*1024, 10*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() || !bytes.Equal(output.Stdout, input) {
		t.Fatalf("status %v, stdout %d bytes", output.Status, len(output.Stdout))
	}
}

// Hard-won: 6dec78fe: A real large-repository resume deadlocked while stdin and stdout pipes were both full.
func TestRunWithInputCompletesWhenChildEchoesInputLargerThanPipeBuffer(t *testing.T) {
	// `cat` echoes stdin to stdout; feeding it well past the typical 64KB
	// pipe buffer reproduces the old deadlock unless stdin is fed
	// concurrently with output drain.
	input := bytes.Repeat([]byte{'x'}, 512*1024)
	output, err := RunWithInput(exec.Command("sh", "-c", "cat"), input)
	if err != nil {
		t.Fatalf("RunWithInput should not deadlock or fail: %v", err)
	}
	if !output.Status.Success() || !bytes.Equal(output.Stdout, input) {
		t.Fatalf("status %v, stdout %d bytes", output.Status, len(output.Stdout))
	}
}

func TestRunWithInputReportsChildStatusWhenChildExitsBeforeReadingAllInput(t *testing.T) {
	// The child exits immediately without reading stdin, so the writer hits
	// a broken pipe partway through writing. That must not surface as a
	// generic write error; the caller should still see the child's real
	// exit status.
	input := bytes.Repeat([]byte{'x'}, 512*1024)
	output, err := RunWithInput(exec.Command("sh", "-c", "exit 3"), input)
	if err != nil {
		t.Fatalf("a broken pipe from an early exit must not be a hard error: %v", err)
	}
	if output.Status.ExitCode() != 3 {
		t.Fatalf("status %v", output.Status)
	}
}

func TestRunCapturingStdoutCollectsMoreThanOnePipeBuffer(t *testing.T) {
	// The child writes well past the 64KB pipe buffer, so a helper that
	// waited before draining would deadlock here.
	output, err := RunCapturingStdout(exec.Command("sh", "-c", "dd if=/dev/zero bs=1024 count=512 2>/dev/null | tr '\\0' 'x'"))
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() || len(output.Stdout) != 512*1024 {
		t.Fatalf("status %v, stdout %d bytes", output.Status, len(output.Stdout))
	}
}

// Hard-won: cd0af915: Dropped detached children stayed zombies and daemon probes treated them as alive.
func TestSpawnDetachedLeavesNoZombieUnderASpawnerThatKeepsRunning(t *testing.T) {
	// This test process outlives the child, which is exactly the case that
	// used to leave a zombie: nothing reaped it, so its process-table entry
	// survived and every existence probe still called it alive.
	pid, err := SpawnDetached(exec.Command("sh", "-c", "exit 0"), filepath.Join(t.TempDir(), "child.log"))
	if err != nil {
		t.Fatalf("SpawnDetached should start the child: %v", err)
	}
	// The reported PID is the real process, not the intermediate, and it is
	// not a child of this process, so init reaps it.
	var status syscall.WaitStatus
	waited, waitErr := syscall.Wait4(int(pid), &status, syscall.WNOHANG, nil)
	if waited != -1 || !errors.Is(waitErr, syscall.ECHILD) {
		t.Fatalf("the detached child must not be our own child: %d %v", waited, waitErr)
	}
	// The grandchild can briefly be a zombie between exit and init's wait.
	// What matters is that its adoptive parent eventually reaps it.
	deadline := time.Now().Add(10 * time.Second)
	for {
		state, present := detachedTestProcessState(pid)
		if !present {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("process %d never left the process table (last state: %c)", pid, state)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

// detachedTestProcessState is the state letter from /proc/<pid>/stat, or
// false once the entry is gone. Other Unixes have no /proc in this shape, so
// there the poll ends as soon as kill(pid, 0) stops finding the process.
func detachedTestProcessState(pid uint32) (byte, bool) {
	if runtime.GOOS == "linux" {
		stat, err := os.ReadFile("/proc/" + strconv.FormatUint(uint64(pid), 10) + "/stat")
		if err != nil {
			return 0, false
		}
		// The comm field can contain spaces and parentheses, so the state
		// letter is the first non-space character after the last ')'.
		end := bytes.LastIndexByte(stat, ')')
		if end < 0 {
			return 0, false
		}
		fields := strings.Fields(string(stat[end+1:]))
		if len(fields) == 0 {
			return 0, false
		}
		return fields[0][0], true
	}
	if syscall.Kill(int(pid), 0) == nil {
		return '?', true
	}
	return 0, false
}

func TestSpawnDetachedReportsExecFailureFromTheGrandchild(t *testing.T) {
	directory := t.TempDir()
	_, err := SpawnDetached(exec.Command(filepath.Join(directory, "missing-executable")), filepath.Join(directory, "child.log"))
	if err == nil {
		t.Fatal("the intermediate exiting must not hide the grandchild's exec failure")
	}
	if !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("not a not-found error: %v", err)
	}
}

// Hard-won: 99a5e469: An inherited macOS pipe descriptor prevented EOF and hung concurrent daemon upgrades.
func TestSpawnDetachedChildKeepsNoDescriptorItsLauncherLeftInheritable(t *testing.T) {
	// A launcher can hold a descriptor without close-on-exec: on macOS a
	// sibling thread's pipe is briefly in that state. If the detached child
	// kept it, whoever reads that pipe would wait for EOF for the child's
	// whole life.
	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	if _, err := unix.FcntlInt(writer.Fd(), unix.F_SETFD, 0); err != nil {
		t.Fatalf("clear FD_CLOEXEC on the write end: %v", err)
	}
	pid, spawned := SpawnDetached(exec.Command("sleep", "30"), filepath.Join(t.TempDir(), "child.log"))
	_ = writer.Close()
	if spawned != nil {
		t.Fatalf("SpawnDetached should start the child: %v", spawned)
	}
	outcome := make(chan error, 1)
	go func() {
		_, err := io.ReadAll(reader)
		outcome <- err
	}()
	var readErr error
	reached := false
	select {
	case readErr = <-outcome:
		reached = true
	case <-time.After(10 * time.Second):
	}
	if err := SignalProcessGroup(int(pid), syscall.SIGKILL); err != nil {
		t.Fatalf("terminate the detached child group: %v", err)
	}
	if !reached {
		t.Fatal("the pipe must reach EOF while the detached child is still running")
	}
	if readErr != nil {
		t.Fatalf("read the pipe to EOF: %v", readErr)
	}
}

func TestSignallingAGroupThatIsAlreadyGoneSucceeds(t *testing.T) {
	// Cancelling a command whose child already exited is the common case; it
	// must not look like a teardown failure.
	command := exec.Command("sh", "-c", "exit 0")
	prepareOwnedGroup(command)
	if err := command.Run(); err != nil {
		t.Fatalf("run short-lived child: %v", err)
	}
	if err := SignalProcessGroup(command.Process.Pid, syscall.SIGKILL); err != nil {
		t.Fatalf("signalling an already-exited process group must succeed: %v", err)
	}
}

func TestSignallingALiveGroupReportsARealFailure(t *testing.T) {
	// An invalid signal number is a caller bug, not a group that already
	// exited, so it must surface instead of being swallowed.
	command := exec.Command("sleep", "30")
	prepareOwnedGroup(command)
	if err := command.Start(); err != nil {
		t.Fatal(err)
	}
	pid := command.Process.Pid
	err := SignalProcessGroup(pid, syscall.Signal(1234))
	if !errors.Is(err, syscall.EINVAL) {
		t.Fatalf("an invalid signal number must be reported: %v", err)
	}
	if err := SignalProcessGroup(pid, syscall.SIGKILL); err != nil {
		t.Fatalf("terminate the test child: %v", err)
	}
	_ = command.Wait()
}

func TestGroupSignalErrorOnlyIgnoresAGoneOwnedGroup(t *testing.T) {
	if !groupSignalErrorIsIgnorable(syscall.ESRCH) {
		t.Fatal("a missing group is gone")
	}
	if groupSignalErrorIsIgnorable(syscall.EINVAL) {
		t.Fatal("an invalid signal is a failure")
	}
	if got, want := groupSignalErrorIsIgnorable(syscall.EPERM), runtime.GOOS == "darwin"; got != want {
		t.Fatalf("EPERM ignorable %v, want %v", got, want)
	}
}

// The descriptor listing is the cloexec path on a kernel without
// close_range, and on macOS. Exercise a descriptor far above 1024 when the
// hard limit permits it, and an ordinary descriptor otherwise. Rust runs the
// listing in the pre-exec child; Go has no pre-exec hook, so it runs here,
// in the process whose descriptors the child would inherit.
func TestDescriptorListingMarksAnInheritedDescriptorCloexec(t *testing.T) {
	if runtime.GOOS != "linux" && runtime.GOOS != "darwin" {
		t.Skip("only Linux and macOS list descriptors")
	}
	RaiseOpenFileLimit()
	var limit syscall.Rlimit
	if err := syscall.Getrlimit(syscall.RLIMIT_NOFILE, &limit); err != nil {
		t.Fatal(err)
	}
	minimum := 3
	if uint64(limit.Cur) > 5000 {
		minimum = 5000
	}
	file, err := os.Open("/dev/null")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	// F_DUPFD chooses an unused descriptor without FD_CLOEXEC, so exec would
	// keep it.
	inherited, err := unix.FcntlInt(file.Fd(), unix.F_DUPFD, minimum)
	if err != nil || inherited < minimum {
		t.Fatalf("dup %d: %v", inherited, err)
	}
	defer unix.Close(inherited)
	if !markListedDescriptorsCloexec() {
		t.Fatal("descriptor listing unavailable")
	}
	if err := exec.Command("sh", "-c", "! [ -e /dev/fd/"+strconv.Itoa(inherited)+" ]").Run(); err != nil {
		t.Fatalf("descriptor %d leaked into the child: %v", inherited, err)
	}
}
