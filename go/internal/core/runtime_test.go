// Port of the tests in mj-core/src/runtime.rs.

package core

import (
	"context"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// Hard-won: 323c8446: Blocking awaits kept polling a dead Tokio timer driver and panicked during shutdown.
func TestShutdownCancelsAnInFlightBlockingAwait(t *testing.T) {
	state := NewShutdownState()
	runtime := NewRuntime()
	started := make(chan struct{})
	result := make(chan bool, 1)
	SpawnBlockingOn(runtime, func() struct{} {
		close(started)
		_, err := BlockOnWith(state, func(ctx context.Context) struct{} {
			select {
			case <-time.After(30 * time.Second):
			case <-ctx.Done():
			}
			return struct{}{}
		})
		result <- err != nil
		return struct{}{}
	})
	<-started
	began := time.Now()
	state.ShutdownWith(runtime, BlockingWorkAwait{}, 5*time.Second)
	if time.Since(began) >= 5*time.Second {
		t.Fatal("shutdown waited for the full grace period")
	}
	if !<-result {
		t.Fatal("the cancelled await should report the shutdown error")
	}
}

func TestBlockOnAfterTheSignalDoesNotPollTheFuture(t *testing.T) {
	state := NewShutdownState()
	state.ShutdownWith(NewRuntime(), BlockingWorkAbandon{}, time.Second)
	var polled atomic.Bool
	_, err := BlockOnWith(state, func(context.Context) struct{} {
		polled.Store(true)
		return struct{}{}
	})
	if err == nil || !strings.Contains(err.Error(), "runtime is shutting down") {
		t.Fatalf("shutting down: %v", err)
	}
	if polled.Load() {
		t.Fatal("the future should not run")
	}
}

// Several blocking waits, each ended only by another task. Without a free
// worker the releasing task never runs and the waits never finish; the
// watchdog then ends them and the timing assertion fails.
// Hard-won: 34e86406: Lifecycle waits occupied every async worker so the daemon could not serve or cancel launches.
func TestBlockingWaitsLeaveTheAsyncWorkersServing(t *testing.T) {
	var release atomic.Bool
	finished := make(chan struct{})
	go func() {
		select {
		case <-finished:
		case <-time.After(10 * time.Second):
			release.Store(true)
		}
	}()
	started := time.Now()
	var waits []*JoinHandle[struct{}]
	for range 3 {
		waits = append(waits, Spawn(func() struct{} {
			return OffAsyncWorker(func() struct{} {
				for !release.Load() {
					time.Sleep(5 * time.Millisecond)
				}
				return struct{}{}
			})
		}))
	}
	// Let every wait start before the releasing task is spawned.
	time.Sleep(100 * time.Millisecond)
	if _, err := Spawn(func() struct{} {
		time.Sleep(10 * time.Millisecond)
		release.Store(true)
		return struct{}{}
	}).Join(context.Background()); err != nil {
		t.Fatal(err)
	}
	for _, wait := range waits {
		if _, err := wait.Join(context.Background()); err != nil {
			t.Fatal(err)
		}
	}
	if elapsed := time.Since(started); elapsed >= 5*time.Second {
		t.Fatalf("blocking waits held every async worker for %v", elapsed)
	}
	close(finished)
}

// Abandon is the dashboard's exit: the grace covers guarded awaits only,
// never a disposable blocking read that would delay quitting.
func TestAbandonDoesNotWaitForUnguardedBlockingWork(t *testing.T) {
	state := NewShutdownState()
	runtime := NewRuntime()
	started := make(chan struct{})
	release := make(chan struct{})
	SpawnBlockingOn(runtime, func() struct{} {
		close(started)
		<-release
		return struct{}{}
	})
	<-started
	began := time.Now()
	state.ShutdownWith(runtime, BlockingWorkAbandon{}, 5*time.Second)
	if elapsed := time.Since(began); elapsed >= 250*time.Millisecond {
		t.Fatalf("abandoning shutdown waited %v", elapsed)
	}
	close(release)
}

// AwaitFor is the daemon's exit: blocking work that finishes in time is
// waited for, and work that does not is left behind and reported rather
// than holding the process open.
// Hard-won: 3c3137a2: Live daemon handoffs stalled after the epilogue while unowned blocking work kept the process open.
func TestAwaitForReportsBlockingWorkItLeftRunning(t *testing.T) {
	state := NewShutdownState()
	stuck := NewRuntime()
	release := make(chan struct{})
	started := make(chan struct{})
	SpawnBlockingOn(stuck, func() struct{} {
		close(started)
		<-release
		return struct{}{}
	})
	<-started
	began := time.Now()
	report := state.ShutdownWith(stuck, BlockingWorkAwaitFor{Limit: 200 * time.Millisecond}, time.Second)
	if elapsed := time.Since(began); elapsed >= 2*time.Second {
		t.Fatalf("bounded shutdown waited %v", elapsed)
	}
	if !report.BlockingLeft {
		t.Fatal("the stuck blocking work was not reported")
	}
	close(release)

	state = NewShutdownState()
	finished := NewRuntime()
	if _, err := SpawnBlockingOn(finished, func() struct{} { return struct{}{} }).Join(context.Background()); err != nil {
		t.Fatal(err)
	}
	report = state.ShutdownWith(finished, BlockingWorkAwaitFor{Limit: 5 * time.Second}, time.Second)
	if report != (ShutdownReport{}) {
		t.Fatalf("report %+v", report)
	}
}
