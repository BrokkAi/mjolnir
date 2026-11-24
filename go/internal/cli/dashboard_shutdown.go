// Port of mj-cli/src/dashboard/shutdown.rs.

package cli

import (
	"sync"
	"time"
)

// timed is one step's output and when it ran.
type timed[T any] struct {
	startedAt  time.Time
	finishedAt time.Time
	output     T
}

// runTimed runs step and records when it ran (Rust's `timed`).
func runTimed[T any](step func() T) timed[T] {
	startedAt := time.Now()
	output := step()
	return timed[T]{startedAt: startedAt, finishedAt: time.Now(), output: output}
}

// overlapFinalCleanup runs independent final persistence and manager
// shutdown together, waiting for both even when either one returns an
// error.
func overlapFinalCleanup[T any](saves func() T, manager func() error) (timed[T], timed[error]) {
	var savesDone timed[T]
	var wait sync.WaitGroup
	wait.Add(1)
	go func() {
		defer wait.Done()
		savesDone = runTimed(saves)
	}()
	managerDone := runTimed(manager)
	wait.Wait()
	return savesDone, managerDone
}
