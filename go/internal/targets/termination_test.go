// Port of the tests in mj-targets/src/termination.rs.

package targets

import (
	"context"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
)

var interruptSuppressionTestLock sync.Mutex

func terminationTestCoordinator() *Coordinator {
	token, cancel := context.WithCancel(context.Background())
	return &Coordinator{token: token, cancel: cancel, signalsSeen: &atomic.Uint32{}}
}

func TestFirstThenRepeatedSignalTransitionsToForce(t *testing.T) {
	var signalsSeen atomic.Uint32
	if action := nextSignalAction(&signalsSeen); action != signalActionGraceful {
		t.Fatalf("first action = %v, want Graceful", action)
	}
	if action := nextSignalAction(&signalsSeen); action != signalActionForce {
		t.Fatalf("second action = %v, want Force", action)
	}
}

func TestSuppressedInterruptDoesNotAdvanceShutdown(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only, as in Rust")
	}
	interruptSuppressionTestLock.Lock()
	defer interruptSuppressionTestLock.Unlock()
	coordinator := terminationTestCoordinator()

	guard := SuppressInterrupts()
	coordinator.receivedSignal(int(syscall.SIGINT))
	if coordinator.Token().Err() != nil {
		t.Fatal("a suppressed interrupt cancelled the coordinator")
	}
	if seen := coordinator.signalsSeen.Load(); seen != 0 {
		t.Fatalf("signals seen = %d, want 0", seen)
	}

	guard.End()
	coordinator.receivedSignal(int(syscall.SIGINT))
	if coordinator.Token().Err() == nil {
		t.Fatal("an unsuppressed interrupt did not cancel the coordinator")
	}
	if seen := coordinator.signalsSeen.Load(); seen != 1 {
		t.Fatalf("signals seen = %d, want 1", seen)
	}
}

func TestCoordinatorCancellationFansOutToLateSubscribers(t *testing.T) {
	interruptSuppressionTestLock.Lock()
	coordinator := terminationTestCoordinator()
	early, cancelEarly := context.WithCancel(coordinator.Token())
	defer cancelEarly()
	coordinator.receivedSignal(0)
	interruptSuppressionTestLock.Unlock()
	late, cancelLate := context.WithCancel(coordinator.Token())
	defer cancelLate()
	<-early.Done()
	<-late.Done()
}
