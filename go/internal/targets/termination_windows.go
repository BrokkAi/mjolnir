// Port of the Windows parts of mj-targets/src/termination.rs.

//go:build windows

package targets

import (
	"context"
	"os"
	"sync"
	"sync/atomic"
)

func installPlatformSignals(coordinator *Coordinator) {
	// Register both handlers before returning from install. Signals arriving
	// before the listener goroutine first runs are then held by the
	// registered channel instead of bypassing coordination.
	ctrlC, ctrlBreak := installWindowsSignals()
	go coordinator.listen(ctrlC, ctrlBreak)
	if coordinator.login {
		installLoginCloseHandler(coordinator.cancel)
	}
}

func (c *Coordinator) listen(ctrlC <-chan os.Signal, ctrlBreak <-chan os.Signal) {
	for {
		select {
		case <-ctrlC:
			c.receivedSignal(0)
		case <-ctrlBreak:
			c.receivedSignal(1)
		}
	}
}

// loginClose is the owner a console close event waits on: it cancels the
// login and holds the process until FinishLogin.
type loginClose struct {
	token    context.CancelFunc
	mu       sync.Mutex
	finished bool
	ready    *sync.Cond
}

// loginCloseOwner is set once, by installLoginCloseHandler.
var loginCloseOwner atomic.Pointer[loginClose]

func installLoginCloseHandler(token context.CancelFunc) {
	panic("unported: mj-targets/src/termination.rs install_login_close_handler")
}

func finishLoginPlatform(c *Coordinator) {
	owner := loginCloseOwner.Load()
	if !c.login || owner == nil {
		return
	}
	owner.mu.Lock()
	owner.finished = true
	owner.mu.Unlock()
	owner.ready.Broadcast()
}

func installWindowsSignals() (ctrlC <-chan os.Signal, ctrlBreak <-chan os.Signal) {
	panic("unported: mj-targets/src/termination.rs install_windows_signals")
}

func exitCode(int) int {
	return 1
}
