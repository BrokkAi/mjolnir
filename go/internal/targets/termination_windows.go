// Port of the Windows parts of mj-targets/src/termination.rs.

//go:build windows

package targets

import (
	"context"
	"fmt"
	"os"
	"sync"
	"sync/atomic"

	"golang.org/x/sys/windows"
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

var procSetConsoleCtrlHandler = windows.NewLazySystemDLL("kernel32.dll").NewProc("SetConsoleCtrlHandler")

// setConsoleCtrlHandler adds handler, a windows.NewCallback, to the
// process's console control handlers. The last one added runs first.
func setConsoleCtrlHandler(handler uintptr) error {
	if added, _, err := procSetConsoleCtrlHandler.Call(handler, 1); added == 0 {
		return err
	}
	return nil
}

func installLoginCloseHandler(token context.CancelFunc) {
	owner := &loginClose{token: token}
	owner.ready = sync.NewCond(&owner.mu)
	if !loginCloseOwner.CompareAndSwap(nil, owner) {
		panic("login close owner installed twice")
	}
	close := windows.NewCallback(func(event uint32) uintptr {
		switch event {
		case windows.CTRL_CLOSE_EVENT, windows.CTRL_LOGOFF_EVENT, windows.CTRL_SHUTDOWN_EVENT:
		default:
			return 0
		}
		owner := loginCloseOwner.Load()
		owner.token()
		owner.mu.Lock()
		for !owner.finished {
			owner.ready.Wait()
		}
		owner.mu.Unlock()
		return 1
	})
	// Windows terminates the process after the close handler returns. Retain
	// that handler until every login preparation helper has been stopped.
	if err := setConsoleCtrlHandler(close); err != nil {
		panic(fmt.Sprintf("install login close handler: %v", err))
	}
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

// installWindowsSignals listens for Ctrl-C and Ctrl-Break separately, as
// tokio's ctrl_c and ctrl_break do; Go's os/signal reports both as one
// interrupt, and only Ctrl-C may be suppressed.
func installWindowsSignals() (ctrlC <-chan os.Signal, ctrlBreak <-chan os.Signal) {
	interrupts := make(chan os.Signal, 1)
	breaks := make(chan os.Signal, 1)
	handler := windows.NewCallback(func(event uint32) uintptr {
		var target chan os.Signal
		switch event {
		case windows.CTRL_C_EVENT:
			target = interrupts
		case windows.CTRL_BREAK_EVENT:
			target = breaks
		default:
			return 0
		}
		// A full channel already holds a wake-up the listener has not taken.
		select {
		case target <- os.Interrupt:
		default:
		}
		return 1
	})
	if err := setConsoleCtrlHandler(handler); err != nil {
		panic(fmt.Sprintf("install Ctrl-C listener: %v", err))
	}
	return interrupts, breaks
}

func exitCode(int) int {
	return 1
}
