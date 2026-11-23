// Port of the Unix parts of mj-targets/src/termination.rs.

//go:build unix

package targets

import (
	"os"
	"os/signal"
	"syscall"
)

func installPlatformSignals(coordinator *Coordinator) {
	installUnixSignals(coordinator)
}

// finishLoginPlatform: only Windows holds a close handler for login.
func finishLoginPlatform(*Coordinator) {}

// installUnixSignals delivers SIGINT and SIGTERM (and SIGHUP for login) to
// the coordinator. The suppression decision is made when the signal is
// delivered; outside login, a signal after the first exits at once with
// 128 + signal, and SIGHUP exits at once.
//
// Rust runs these decisions in signal handlers and wakes a listener thread
// through a self-pipe; Go's os/signal already delivers signals to a
// goroutine, which makes the same decisions in the same order: an armed
// request (or, outside login, a hangup) exits before suppression is
// consulted, and only the first unsuppressed signal reaches the coordinator.
func installUnixSignals(coordinator *Coordinator) {
	delivered := make(chan os.Signal, 8)
	// Outside login SIGHUP is registered too, so it exits with 128 + SIGHUP
	// rather than Go's default termination.
	signal.Notify(delivered, syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
	go func() {
		requested := false
		for received := range delivered {
			number, _ := received.(syscall.Signal)
			if !coordinator.login && (requested || number == syscall.SIGHUP) {
				os.Exit(exitCode(int(number)))
			}
			if number == syscall.SIGINT && suppressedInterrupts.Load() > 0 {
				continue
			}
			if !requested {
				requested = true
				coordinator.receivedSignal(0)
			}
		}
	}()
}

func exitCode(signal int) int {
	return 128 + signal
}
