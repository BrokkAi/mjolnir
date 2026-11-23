// Port of the tests in mj-store/src/worker_lifecycle.rs.

package store

import (
	"context"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestNestedLifecycleCallsBorrowTheSameOperationAndRejectOtherSessions(t *testing.T) {
	ctx := context.Background()
	gate := core.NewRecoveryGate()
	admission, err := gate.EnterForeground(ctx, "nested", "test lifecycle", core.LifecycleOriginPerson, new(atomic.Bool), core.ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	defer admission.Release()
	owner, err := WorkerPermitAcquire(ctx, "nested", admission)
	if err != nil {
		t.Fatal(err)
	}
	defer owner.Release()
	_, err = Run(owner.Scope(ctx), "nested", "nested async", core.ProcessExecutor{}, func(ctx context.Context) (struct{}, error) {
		nested, err := Require(ctx, "nested")
		if err != nil {
			return struct{}{}, err
		}
		if nested.OperationID() != owner.OperationID() {
			t.Fatalf("async scope operation %s, want %s", nested.OperationID(), owner.OperationID())
		}
		if _, err := RunBlocking(ctx, "nested", "nested blocking", core.ProcessExecutor{}, func(ctx context.Context) (struct{}, error) {
			blocking, err := Require(ctx, "nested")
			if err != nil {
				return struct{}{}, err
			}
			if blocking.OperationID() != owner.OperationID() {
				t.Fatalf("blocking scope operation %s, want %s", blocking.OperationID(), owner.OperationID())
			}
			return struct{}{}, nil
		}); err != nil {
			return struct{}{}, err
		}
		_, failure := Run(ctx, "other", "cross-session helper", core.ProcessExecutor{}, func(context.Context) (struct{}, error) {
			return struct{}{}, nil
		})
		if failure == nil || !strings.Contains(failure.Error(), "cross-session helper for session other") {
			t.Fatalf("cross-session helper: %v", failure)
		}
		return struct{}{}, nil
	})
	if err != nil {
		t.Fatal(err)
	}
	_, failure := Run(ctx, "nested", "missing owner helper", core.ProcessExecutor{}, func(context.Context) (struct{}, error) {
		return struct{}{}, nil
	})
	if failure == nil || !strings.Contains(failure.Error(), "missing owner helper for session nested") {
		t.Fatalf("missing owner: %v", failure)
	}
}

// Hard-won: a6ad3cc3: aborting a waiter released ownership during an executing blocking swap.
func TestDroppingAnExecutorWaiterCannotReleaseAnExecutingSwap(t *testing.T) {
	ctx := context.Background()
	gate := core.NewRecoveryGate()
	admission, refusal := gate.TryStart("swap", core.BackgroundWorkWorkerReplacement)
	if refusal != nil {
		t.Fatalf("refused: %+v", refusal)
	}
	owner, err := WorkerPermitAcquire(ctx, "swap", admission)
	if err != nil {
		t.Fatal(err)
	}
	admission.Release()
	started := make(chan struct{})
	release := make(chan struct{})
	executing := core.SpawnBlocking(func() struct{} {
		// The blocking work owns the permit and gives it up when it returns.
		defer owner.Release()
		_ = owner.ScopeBlocking(ctx)
		close(started)
		select {
		case <-release:
		case <-time.After(5 * time.Second):
			panic("the test never released the swap")
		}
		return struct{}{}
	})
	<-started
	// The waiter goes away; the executing swap keeps its ownership.
	waiter, cancel := context.WithCancel(ctx)
	cancel()
	if _, err := executing.Join(waiter); err == nil {
		t.Fatal("a cancelled waiter must not observe the swap finishing")
	}
	if attempt, refusal := gate.TryStart("swap", core.BackgroundWorkRelayRecovery); refusal == nil {
		attempt.Release()
		t.Fatal("the executing swap must still own the session")
	}
	close(release)
	if _, err := executing.Join(ctx); err != nil {
		t.Fatal(err)
	}
	attempt, refusal := gate.TryStart("swap", core.BackgroundWorkRelayRecovery)
	if refusal != nil {
		t.Fatalf("the finished swap still owns the session: %+v", refusal)
	}
	attempt.Release()
}
