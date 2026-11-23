// Port of mj-accounts/src/login_terminal.rs.

// Daemon-owned disposable login terminals. No worker state or handoff
// admission.

package accounts

import (
	"context"
	"io"
	"net"
	"os/exec"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// loginProcess is the login root and whether this owner still has to stop
// it.
type loginProcess struct {
	child *exec.Cmd
	armed bool
}

// terminate kills the root's process group, once. The root is not reaped
// until cleanup, so its process-group id cannot be recycled while this owner
// still has terminal reader/writer tasks.
func (p *loginProcess) terminate() {
	panic("unported: mj-accounts/src/login_terminal.rs Process::terminate")
}

// hangup wakes the root's cleanup owner before waiting for its exit:
// reader/writer duplicates keep the master open. Unix only; a no-op
// elsewhere.
func (p *loginProcess) hangup() {
	panic("unported: mj-accounts/src/login_terminal.rs Process::hangup")
}

// release is Rust's Drop: an owner abandoned while armed hangs up the root
// and transfers its cleanup to an independent goroutine, never blocking the
// daemon or killing the root first.
func (p *loginProcess) release() {
	panic("unported: mj-accounts/src/login_terminal.rs Process::drop")
}

// loginPty is the master side of the login terminal: its output, its input
// and its size.
type loginPty interface {
	io.ReadWriteCloser
	Resize(width, height int) error
}

// loginMaster holds the PTY master until it is closed. On Windows closing
// it is moved off the caller (Rust's Drop): ConPTY close can wait for the
// login close handler.
type loginMaster struct {
	mu     sync.Mutex
	master loginPty
}

// release is Rust's Drop for Master.
func (m *loginMaster) release() {
	panic("unported: mj-accounts/src/login_terminal.rs Master::drop")
}

// closeConsoleOnExit closes the console when the root exits, on Windows:
// closing ConPTY sends CTRL_CLOSE_EVENT. The login handler retains cleanup
// ownership until preparation groups stop. Output is drained concurrently:
// ClosePseudoConsole can emit a final frame and block. Nil elsewhere.
// https://learn.microsoft.com/en-us/windows/console/closepseudoconsole
func closeConsoleOnExit(master *loginMaster) *core.JoinHandle[struct{}] {
	panic("unported: mj-accounts/src/login_terminal.rs close_console_on_exit")
}

// loginTerminalSize is the PTY size for cols and rows.
func loginTerminalSize(cols, rows uint16) (width, height int) {
	return int(cols), int(rows)
}

// LoginTerminalServe serves a prepared command on stream until the login
// exits or the connection ends. Tests pass a real fixture command through
// this same connection executor; production passes this daemon's login
// entrypoint. Cancelling ctx is the daemon stopping or handing off (Rust's
// `cancellation` token).
func LoginTerminalServe(ctx context.Context, stream net.Conn, command *exec.Cmd, cols, rows uint16) error {
	panic("unported: mj-accounts/src/login_terminal.rs serve")
}
