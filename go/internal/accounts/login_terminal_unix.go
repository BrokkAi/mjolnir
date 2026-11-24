//go:build unix

// Port of the Unix parts of mj-accounts/src/login_terminal.rs.

package accounts

import (
	"errors"
	"log/slog"
	"os/exec"
	"syscall"

	"github.com/charmbracelet/x/xpty"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// loginConsoleClosesOnExit: only ConPTY needs its console closed when the
// root exits.
const loginConsoleClosesOnExit = false

// startLoginTerminal opens the PTY and starts command on it as a session
// leader with the terminal as its controlling terminal, as portable-pty's
// spawn_command does. The parent keeps only the master.
func startLoginTerminal(command *exec.Cmd, cols, rows uint16) (loginPty, *loginProcess, error) {
	pty, err := xpty.NewUnixPty(loginTerminalSize(cols, rows))
	if err != nil {
		return nil, nil, err
	}
	slave := pty.Slave()
	command.Stdin, command.Stdout, command.Stderr = slave, slave, slave
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	command.SysProcAttr.Setsid = true
	command.SysProcAttr.Setctty = true
	// The controlling terminal is the child's stdin.
	command.SysProcAttr.Ctty = 0
	if err := command.Start(); err != nil {
		_ = pty.Close()
		return nil, nil, err
	}
	// Rust drops the slave once the child holds it, so the master reports
	// EIO when the last process on the terminal exits.
	_ = slave.Close()
	return &loginUnixMaster{pty: pty}, &loginProcess{child: command, armed: true}, nil
}

// loginUnixMaster is the master alone: the slave was closed after the
// spawn, so closing the PTY closes only the master.
type loginUnixMaster struct {
	pty *xpty.UnixPty
}

func (m *loginUnixMaster) Read(buffer []byte) (int, error)  { return m.pty.Master().Read(buffer) }
func (m *loginUnixMaster) Write(buffer []byte) (int, error) { return m.pty.Master().Write(buffer) }
func (m *loginUnixMaster) Close() error                     { return m.pty.Master().Close() }
func (m *loginUnixMaster) Resize(width, height int) error   { return m.pty.Resize(width, height) }

// loginReadEnded: Linux reports EIO when the last PTY slave closes.
func loginReadEnded(err error) bool {
	return errors.Is(err, syscall.EIO)
}

func loginKillGroup(pid int) {
	core.TerminateProcessGroup(pid, syscall.SIGKILL)
}

func loginHangup(pid int) {
	// Reader/writer duplicates keep the master open, so explicitly wake the
	// root's cleanup owner before waiting for its exit.
	if err := syscall.Kill(pid, syscall.SIGHUP); err != nil {
		slog.Debug("hang up login root", "error", err)
	}
}

// loginAbandonedCleanup waits for an abandoned root's exit without reaping
// it, then kills what it left in its group.
func loginAbandonedCleanup(pid int) {
	if err := waitForExit(pid); err != nil {
		slog.Warn("observe abandoned login cleanup", "error", err)
	}
	core.TerminateProcessGroup(pid, syscall.SIGKILL)
}

// loginWaitChild reaps the root and reports its exit code as portable-pty
// does: a root killed by a signal exited with 1.
func loginWaitChild(child *exec.Cmd) (uint32, error) {
	state, err := child.Process.Wait()
	if err != nil {
		return 0, err
	}
	if status, ok := state.Sys().(syscall.WaitStatus); ok && status.Signaled() {
		return 1, nil
	}
	return uint32(state.ExitCode()), nil
}

// observeExit observes the root's exit without reaping it: the process id
// remains reserved until the terminal owner has signaled the group and
// drained the PTY.
func observeExit(process *loginProcess) (*core.JoinHandle[error], error) {
	if process.child == nil || process.child.Process == nil {
		return nil, errors.New("login child has no process id")
	}
	pid := process.child.Process.Pid
	return core.SpawnBlocking(func() error { return waitForExit(pid) }), nil
}
