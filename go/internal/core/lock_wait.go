// Port of mj-core/src/lock_wait.rs.
//
// Reporting for a poll loop that waits on a busy lock, so a long wait names
// the lock, the waiter and the holder instead of staying silent.

package core

import (
	"log/slog"
	"time"
)

// lockWaitFirstReport: a wait this short is ordinary contention and is not
// reported.
const lockWaitFirstReport = 5 * time.Second

// lockWaitRepeatReport: a wait that keeps going repeats its line at this
// interval.
const lockWaitRepeatReport = 60 * time.Second

// LockWait is one wait on one lock. Create it when the first attempt finds
// the lock busy, call Blocked on every failed attempt, and end it with
// Acquired. Ending it unfinished (cancellation or an error) must call Drop,
// which ends the wait as "gave up".
type LockWait struct {
	subject    string
	started    time.Time
	nextReport time.Duration
	reports    uint32
	// Captured at the first report so the closing line names the same lock.
	named       bool
	namedLock   string
	namedWaiter string
}

// NewLockWait: subject completes the sentence "waiting for ..." in the log
// lines.
func NewLockWait(subject string) *LockWait {
	return &LockWait{subject: subject, started: time.Now(), nextReport: lockWaitFirstReport}
}

// due reports whether a line is due after elapsed, and schedules the next.
func (w *LockWait) due(elapsed time.Duration) bool {
	if elapsed < w.nextReport {
		return false
	}
	w.nextReport = elapsed + lockWaitRepeatReport
	return true
}

// Blocked: an attempt found the lock busy. holder is only evaluated when a
// line is due.
func (w *LockWait) Blocked(lock, waiter string, holder func() string) {
	elapsed := time.Since(w.started)
	if !w.due(elapsed) {
		return
	}
	message := "waiting for " + w.subject
	if w.reports > 0 {
		message = "still waiting for " + w.subject
	}
	slog.Info(message, "lock", lock, "waiter", waiter, "holder", holder(), "waited_seconds", uint64(elapsed/time.Second))
	w.reports++
	w.named, w.namedLock, w.namedWaiter = true, lock, waiter
}

// Acquired: the lock was taken.
func (w *LockWait) Acquired() {
	w.finish("acquired")
}

func (w *LockWait) finish(outcome string) {
	if !w.named {
		return
	}
	w.named = false
	slog.Info("finished waiting for "+w.subject, "lock", w.namedLock, "waiter", w.namedWaiter,
		"waited_seconds", uint64(time.Since(w.started)/time.Second), "outcome", outcome)
}

// Drop is Rust's Drop: an unfinished wait ends as "gave up". It does nothing
// after Acquired.
func (w *LockWait) Drop() {
	w.finish("gave up")
}
