// Port of mj-targets/src/termination.rs.
//
// Process-wide graceful termination coordination.
//
// The first termination signal cancels all subscribers so callers can unwind
// through their normal cleanup paths. A second signal exits immediately. On
// Unix the immediate exit status follows the conventional `128 + signal`
// convention (SIGINT 130, SIGHUP 129, SIGTERM 143); Windows uses 1.
//
// Rust's CancellationToken is a context.Context with its CancelFunc here.

package targets

import (
	"context"
	"os"
	"runtime"
	"sync/atomic"
	"syscall"
)

type signalAction int

const (
	signalActionGraceful signalAction = iota
	signalActionForce
)

var suppressedInterrupts atomic.Int64

// SuppressInterruptGuard keeps a foreground child process's Ctrl-C from also
// terminating Hel.
//
// The child remains in the terminal's foreground process group and receives
// the signal normally; only Mjolnir's process-wide graceful shutdown is
// suspended until the guard ends.
type SuppressInterruptGuard struct {
	ended atomic.Bool
}

func SuppressInterrupts() *SuppressInterruptGuard {
	suppressedInterrupts.Add(1)
	return &SuppressInterruptGuard{}
}

// End is Rust's Drop: interrupts reach Mjolnir again once every guard has
// ended. Ending a guard twice has no further effect.
func (g *SuppressInterruptGuard) End() {
	if g.ended.CompareAndSwap(false, true) {
		suppressedInterrupts.Add(-1)
	}
}

// nextSignalAction is the pure, testable signal transition. The side effect
// for Force belongs to the listener, never to this state machine.
func nextSignalAction(signalsSeen *atomic.Uint32) signalAction {
	if signalsSeen.Add(1)-1 == 0 {
		return signalActionGraceful
	}
	return signalActionForce
}

type Coordinator struct {
	token       context.Context
	cancel      context.CancelFunc
	signalsSeen *atomic.Uint32
	login       bool
}

func CoordinatorInstall() *Coordinator {
	return coordinatorInstallMode(false)
}

// CoordinatorInstallLogin: login owns preparation groups and must unwind on
// hangup, including repeated signals. The dashboard retains its immediate
// hangup behavior.
func CoordinatorInstallLogin() *Coordinator {
	return coordinatorInstallMode(true)
}

func coordinatorInstallMode(login bool) *Coordinator {
	token, cancel := context.WithCancel(context.Background())
	coordinator := &Coordinator{
		token:       token,
		cancel:      cancel,
		signalsSeen: &atomic.Uint32{},
		login:       login,
	}
	// installPlatformSignals is install_unix_signals on Unix and the
	// Windows listener (with the login close handler) on Windows.
	installPlatformSignals(coordinator)
	return coordinator
}

// Token is cancelled by the first termination signal.
func (c *Coordinator) Token() context.Context {
	return c.token
}

func (c *Coordinator) receivedSignal(signal int) {
	if c.login {
		c.cancel()
		return
	}
	unix := runtime.GOOS != "windows"
	if unix && signal == int(syscall.SIGINT) && suppressedInterrupts.Load() > 0 {
		return
	}
	if !unix && signal == 0 && suppressedInterrupts.Load() > 0 {
		return
	}
	// SIGHUP means the controlling terminal is gone. A graceful cancel cannot
	// work then: the UI may never observe the token on the dead tty and the
	// process survives as a headless CPU spinner. Exit immediately; detached
	// workers are unaffected and child proxies exit on EOF.
	if unix && signal == int(syscall.SIGHUP) {
		os.Exit(exitCode(signal))
	}
	switch nextSignalAction(c.signalsSeen) {
	case signalActionGraceful:
		c.cancel()
	case signalActionForce:
		os.Exit(exitCode(signal))
	}
}

func (c *Coordinator) FinishLogin() {
	finishLoginPlatform(c)
}
