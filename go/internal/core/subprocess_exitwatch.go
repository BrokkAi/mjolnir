//go:build !linux

// Port of mj-core/src/subprocess.rs: ExitWatch where there is no pidfd.
// Every check reports a possible exit, so callers reap on each tick.

package core

import (
	"os"
	"time"
)

// NewExitWatch must be called before the child is reaped, so the pid cannot
// be reused.
func NewExitWatch(*os.Process) *ExitWatch { return &ExitWatch{pidfd: -1} }

// Wait sleeps up to timeout and reports a possible exit.
func (w *ExitWatch) Wait(timeout time.Duration) bool {
	if timeout > 0 {
		time.Sleep(timeout)
	}
	return true
}

// Close releases nothing: there is no pidfd.
func (w *ExitWatch) Close() {}
