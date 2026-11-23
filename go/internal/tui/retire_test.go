// Port of mj-tui/src/retire.rs tests.

package tui

import (
	"runtime"
	"testing"
	"time"
)

// retireReportsItsRelease is a value whose release the collector reports.
type retireReportsItsRelease struct {
	payload []byte
}

// TestARetiredValueIsDroppedOnAnotherThread: Rust proves the drop happens on
// the retire thread. In Go the collector frees an unreachable value on its
// own workers; the cleanup below sends on an unbuffered channel, so had it run
// on this goroutine (the event loop's stand-in) it could never complete.
func TestARetiredValueIsDroppedOnAnotherThread(t *testing.T) {
	released := make(chan struct{})
	value := &retireReportsItsRelease{payload: make([]byte, 64)}
	runtime.AddCleanup(value, func(released chan struct{}) { released <- struct{}{} }, released)
	retire(value)
	value = nil
	deadline := time.After(5 * time.Second)
	for {
		runtime.GC()
		select {
		case <-released:
			return
		case <-time.After(10 * time.Millisecond):
		case <-deadline:
			t.Fatal("the retired value is dropped")
		}
	}
}
