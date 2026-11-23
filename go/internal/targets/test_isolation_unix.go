// Port of the Unix parts of mj-targets/src/test_isolation.rs.

//go:build unix

package targets

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// IncludeIgnored includes an ignored integration case in the isolated child
// process (see IncludeIgnoredEnv).
func (t *IsolatedTest) IncludeIgnored() *IsolatedTest {
	return t.Env(IncludeIgnoredEnv, "1")
}

// Output runs the child and returns its output without judging it.
func (t *IsolatedTest) Output() core.ProcessOutput {
	return t.outputUnix()
}

func (t *IsolatedTest) outputUnix() core.ProcessOutput {
	command := t.command
	command.Env = t.environment()
	command.Stdin = nil
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Pgid: 0}
	stdoutRead, stdoutWrite, err := os.Pipe()
	if err != nil {
		panic(fmt.Sprintf("run isolated %s: %v", t.name, err))
	}
	stderrRead, stderrWrite, err := os.Pipe()
	if err != nil {
		panic(fmt.Sprintf("run isolated %s: %v", t.name, err))
	}
	command.Stdout = stdoutWrite
	command.Stderr = stderrWrite
	err = command.Start()
	_ = stdoutWrite.Close()
	_ = stderrWrite.Close()
	if err != nil {
		_ = stdoutRead.Close()
		_ = stderrRead.Close()
		panic(fmt.Sprintf("run isolated %s: %v", t.name, err))
	}
	guard := newIsolatedProcessGroupGuard(command.Process.Pid)
	defer guard.close()
	type collected struct {
		bytes []byte
		err   error
	}
	readAll := func(pipe *os.File) <-chan collected {
		done := make(chan collected, 1)
		go func() {
			defer pipe.Close()
			bytes, err := io.ReadAll(pipe)
			done <- collected{bytes, err}
		}()
		return done
	}
	stdoutReader := readAll(stdoutRead)
	stderrReader := readAll(stderrRead)
	// Wait for the isolated child to exit without reaping it yet so its PID
	// (and therefore its process group ID) cannot be recycled while we
	// terminate any surviving members of that process group.
	waitForExitWithoutReaping(command.Process.Pid)
	guard.terminate()
	waitErr := command.Wait()
	var exited *exec.ExitError
	if waitErr != nil && !errors.As(waitErr, &exited) {
		panic(fmt.Sprintf("wait for isolated %s: %v", t.name, waitErr))
	}
	stdout := <-stdoutReader
	if stdout.err != nil {
		panic(fmt.Sprintf("read isolated %s stdout: %v", t.name, stdout.err))
	}
	stderr := <-stderrReader
	if stderr.err != nil {
		panic(fmt.Sprintf("read isolated %s stderr: %v", t.name, stderr.err))
	}
	return core.ProcessOutput{Status: command.ProcessState, Stdout: stdout.bytes, Stderr: stderr.bytes}
}

type isolatedProcessGroupGuard struct {
	// pgid is the child's process group, or 0 once terminated or for none.
	pgid int
}

func newIsolatedProcessGroupGuard(pid int) *isolatedProcessGroupGuard {
	if pid <= 1 {
		pid = 0
	}
	return &isolatedProcessGroupGuard{pgid: pid}
}

func (g *isolatedProcessGroupGuard) terminate() {
	pgid := g.pgid
	g.pgid = 0
	if pgid == 0 {
		return
	}
	core.TerminateProcessGroup(pgid, syscall.SIGKILL)
}

// close is Rust's Drop.
func (g *isolatedProcessGroupGuard) close() {
	g.terminate()
}
