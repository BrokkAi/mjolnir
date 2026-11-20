//go:build linux

// Port of the Linux tests in mj-core/src/subprocess.rs.

package core

import (
	"context"

	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func TestExitWatchWakesWhenTheChildExitsAndNotBefore(t *testing.T) {
	// The child stays alive until its stdin closes, so the first check
	// cannot race its exit.
	command := exec.Command("sh", "-c", "read line")
	stdin, err := command.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := command.Start(); err != nil {
		t.Fatal(err)
	}
	watch := NewExitWatch(command.Process)
	defer watch.Close()
	if watch.pidfd < 0 {
		// Kernel or seccomp without pidfd_open: the documented fallback
		// reports a possible exit on every check, so nothing to observe.
		_ = stdin.Close()
		_ = command.Wait()
		return
	}
	if watch.Wait(0) {
		t.Fatal("a running child must not look exited")
	}
	_ = stdin.Close()
	started := time.Now()
	if !watch.Wait(10 * time.Second) {
		t.Fatal("the child exited")
	}
	if time.Since(started) >= 5*time.Second {
		t.Fatal("the wait must end at the exit, not at its timeout")
	}
	var info unix.Siginfo
	if err := unix.Waitid(unix.P_PID, command.Process.Pid, &info, unix.WEXITED|unix.WNOHANG|unix.WNOWAIT, nil); err != nil {
		t.Fatal(err)
	}
	if info.Signo == 0 {
		t.Fatal("the child has exited")
	}
	_ = command.Wait()
}

// The leader's exit completes a bounded command; a descendant that keeps the
// pipes open is stopped after the drain instead of failing the command at
// its timeout.
// Hard-won: c86b2123: A completed command stayed Running until a background descendant closed inherited pipes.
func TestBoundedCaptureCompletesAtLeaderExitWhenADescendantHoldsThePipes(t *testing.T) {
	pidFile := filepath.Join(t.TempDir(), "descendant")
	started := time.Now()
	output, err := RunBounded(context.Background(),
		exec.Command("sh", "-c", "sleep 300 & echo $! > \"$1\"; echo hi", "held-pipes", pidFile), 100_000, 30*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if time.Since(started) >= 5*time.Second {
		t.Fatalf("took %v", time.Since(started))
	}
	if string(output.Stdout) != "hi\n" {
		t.Fatalf("stdout %q", output.Stdout)
	}
	body, err := os.ReadFile(pidFile)
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(body)))
	if err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(2 * time.Second)
	for {
		stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
		if err != nil {
			return
		}
		if _, fields, ok := strings.Cut(string(stat), ") "); ok && strings.HasPrefix(fields, "Z") {
			return
		}
		if time.Now().After(deadline) {
			_ = syscall.Kill(pid, syscall.SIGKILL)
			t.Fatalf("descendant %d survived the completed command", pid)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// Hard-won: cd0af9150: Linux published the exec'd child's comm after spawn returned.
func TestSpawnDetachedReportsTheRealChildAndLeavesItLeadingItsOwnGroup(t *testing.T) {
	// Callers signal the returned PID's process group to tear the child
	// down, so the PID has to be the exec'd program rather than the
	// short-lived intermediate, and it has to lead that group.
	executable, err := filepath.EvalSymlinks("/bin/sleep")
	if err != nil {
		t.Fatalf("resolve sleep executable: %v", err)
	}
	pid, err := SpawnDetached(exec.Command(executable, "30"), filepath.Join(t.TempDir(), "child.log"))
	if err != nil {
		t.Fatalf("SpawnDetached should start the child: %v", err)
	}
	group := NewProcessGroupGuard(int(pid))
	defer group.Close()
	// Linux installs mm->exe_file before closing the exec-status channel,
	// but updates comm afterwards. Check the executable identity owned by
	// the new address space, not the independently published display name.
	actual, err := os.Readlink(fmt.Sprintf("/proc/%d/exe", pid))
	if err != nil {
		t.Fatalf("the reported pid must name a live process: %v", err)
	}
	if actual != executable {
		t.Fatalf("executable %s, want %s", actual, executable)
	}
	leader, err := syscall.Getpgid(int(pid))
	if err != nil || leader != int(pid) {
		t.Fatalf("the child must lead its own process group: %d %v", leader, err)
	}
}

// The capture helper moves its child into a separate group without losing
// interactive terminal input to SIGTTIN.
func TestInteractiveCaptureKeepsTerminalInputInItsOwnedGroup(t *testing.T) {
	const child = "MJ_TEST_INTERACTIVE_CAPTURE_CHILD"
	if os.Getenv(child) != "" {
		result, err := RunCapturingStdoutCancellable(context.Background(), exec.Command("/bin/sh", "-c",
			`read -r answer; head -c 131072 /dev/zero; printf '%s' "$answer"`))
		if err != nil {
			t.Fatal(err)
		}
		if !result.Status.Success() {
			t.Fatalf("status %v", result.Status)
		}
		if len(result.Stdout) != 131072+len("terminal-answer") {
			t.Fatalf("stdout %d bytes", len(result.Stdout))
		}
		for _, b := range result.Stdout[:131072] {
			if b != 0 {
				t.Fatal("the zeros come first")
			}
		}
		if !strings.HasSuffix(string(result.Stdout), "terminal-answer") {
			t.Fatal("the terminal's answer comes last")
		}
		return
	}
	master, slave := subprocessOpenPTY(t)
	defer master.Close()
	command := exec.Command(os.Args[0], "-test.run", "^TestInteractiveCaptureKeepsTerminalInputInItsOwnedGroup$", "-test.v")
	command.Env = append(os.Environ(), child+"=1")
	command.Stdin, command.Stdout, command.Stderr = slave, nil, os.Stderr
	// A session of its own with the terminal as its controlling terminal.
	command.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Setctty: true, Ctty: 0}
	if err := command.Start(); err != nil {
		t.Fatal(err)
	}
	_ = slave.Close()
	if _, err := master.Write([]byte("terminal-answer\n")); err != nil {
		t.Fatal(err)
	}
	// Read the terminal's output (the echoed answer) as a real terminal does.
	// An unread master can keep an exiting session leader waiting.
	drained := make(chan struct{})
	go func() {
		defer close(drained)
		buffer := make([]byte, 1024)
		for {
			if _, err := master.Read(buffer); err != nil {
				// Linux reports a terminal whose slave side closed as EIO.
				return
			}
		}
	}()
	if err := command.Wait(); err != nil {
		t.Fatalf("the interactive child failed: %v", err)
	}
	_ = master.Close()
	<-drained
}

// subprocessOpenPTY opens a pseudo-terminal pair, as openpty does.
func subprocessOpenPTY(t *testing.T) (*os.File, *os.File) {
	t.Helper()
	master, err := os.OpenFile("/dev/ptmx", os.O_RDWR|unix.O_NOCTTY, 0)
	if err != nil {
		t.Fatal(err)
	}
	if err := unix.IoctlSetPointerInt(int(master.Fd()), unix.TIOCSPTLCK, 0); err != nil {
		t.Fatal(err)
	}
	number, err := unix.IoctlGetInt(int(master.Fd()), unix.TIOCGPTN)
	if err != nil {
		t.Fatal(err)
	}
	slave, err := os.OpenFile(fmt.Sprintf("/dev/pts/%d", number), os.O_RDWR|unix.O_NOCTTY, 0)
	if err != nil {
		t.Fatal(err)
	}
	return master, slave
}
