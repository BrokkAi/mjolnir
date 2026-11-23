// Port of the tests in mj-core/src/recovery_gate.rs.

package core

import (
	"context"
	"errors"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"testing/synctest"
	"time"
)

func recoveryStart(t *testing.T, gate *RecoveryGate, sessionID string, kind BackgroundWork) *RecoveryAttempt {
	t.Helper()
	attempt, refusal := gate.TryStart(sessionID, kind)
	if refusal != nil {
		t.Fatalf("start %s %s: refused by %q", sessionID, kind.Label(), refusal.Holders)
	}
	return attempt
}

func recoveryRefused(t *testing.T, gate *RecoveryGate, sessionID string, kind BackgroundWork) *RecoveryRefusal {
	t.Helper()
	attempt, refusal := gate.TryStart(sessionID, kind)
	if refusal == nil {
		attempt.Release()
		t.Fatalf("start %s %s was admitted", sessionID, kind.Label())
	}
	return refusal
}

func TestLifecycleReservationPreemptsBackgroundWorkBeforeReleasingAdmission(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		gate := NewRecoveryGate()
		started := make(chan struct{})
		result := make(chan bool, 1)
		go func() {
			_, ok := RecoveryRunBackground(gate, context.Background(), "child", BackgroundWorkDelegationDelivery, func(ctx context.Context) struct{} {
				close(started)
				<-ctx.Done()
				return struct{}{}
			})
			result <- ok
		}()
		<-started
		reservation := gate.Reserve("child")
		if !gate.IsBusy("child") {
			t.Fatal("background work is not busy")
		}
		gate.CancelBusy("child")
		select {
		case ok := <-result:
			if ok {
				t.Fatal("preempted work reported a result")
			}
		case <-time.After(time.Second):
			t.Fatal("preempted work did not stop")
		}
		if gate.IsBusy("child") {
			t.Fatal("preempted work still busy")
		}
		if _, ok := RecoveryRunBackground(gate, context.Background(), "child", BackgroundWorkDelegationDelivery, func(context.Context) struct{} {
			panic("reserved worker accessed")
		}); ok {
			t.Fatal("reserved session admitted background work")
		}
		if value, ok := RecoveryRunBackground(gate, context.Background(), "other", BackgroundWorkDelegationDelivery, func(context.Context) int { return 7 }); !ok || value != 7 {
			t.Fatalf("other session = %d, %v", value, ok)
		}
		reservation.Release()
		if value, ok := RecoveryRunBackground(gate, context.Background(), "child", BackgroundWorkDelegationDelivery, func(context.Context) int { return 9 }); !ok || value != 9 {
			t.Fatalf("released session = %d, %v", value, ok)
		}
	})
}

func TestAbortingBackgroundTaskReleasesWorkerAdmission(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		gate := NewRecoveryGate()
		started := make(chan struct{})
		ctx, cancel := context.WithCancel(context.Background())
		result := make(chan bool, 1)
		go func() {
			_, ok := RecoveryRunBackground(gate, ctx, "child", BackgroundWorkDelegationDelivery, func(ctx context.Context) struct{} {
				close(started)
				<-ctx.Done()
				return struct{}{}
			})
			result <- ok
		}()
		<-started
		if _, ok := RecoveryRunBackground(gate, context.Background(), "child", BackgroundWorkDelegationDelivery, func(context.Context) int { return 1 }); ok {
			t.Fatal("a held session admitted background work")
		}
		cancel()
		if <-result {
			t.Fatal("the aborted task reported a result")
		}
		if value, ok := RecoveryRunBackground(gate, context.Background(), "child", BackgroundWorkDelegationDelivery, func(context.Context) int { return 2 }); !ok || value != 2 {
			t.Fatalf("released session = %d, %v", value, ok)
		}
	})
}

func TestClosingGateCancelsAdmittedWorkAndRefusesLateAdmission(t *testing.T) {
	for range 100 {
		gate := NewRecoveryGate()
		started := make(chan *RecoveryAttempt, 1)
		go func() {
			attempt, _ := gate.TryStart("session", BackgroundWorkDelegationDelivery)
			started <- attempt
		}()
		gate.Close()
		if attempt := <-started; attempt != nil {
			if !attempt.Cancelled() {
				t.Fatal("closing did not cancel admitted work")
			}
			if !gate.IsBusy("session") {
				t.Fatal("admitted work lost its slot before settling")
			}
			attempt.Release()
		}
		recoveryRefused(t, gate, "late", BackgroundWorkDelegationDelivery)
		if sessions := gate.BusySessions(); len(sessions) != 0 {
			t.Fatalf("busy = %q", sessions)
		}
	}
}

func TestOldAttemptIdentityCannotReleaseAReplacement(t *testing.T) {
	gate := NewRecoveryGate()
	old := recoveryStart(t, gate, "session", BackgroundWorkDelegationDelivery)
	identity := old.Cancellation()
	old.Release()
	replacement := recoveryStart(t, gate, "session", BackgroundWorkDelegationDelivery)
	gate.finish("session", identity)
	if !gate.IsBusy("session") {
		t.Fatal("an old identity released the replacement")
	}
	replacement.Release()
	if gate.IsBusy("session") {
		t.Fatal("the replacement did not release")
	}
}

func TestBusyPublicationTracksConcurrentSessionTransitions(t *testing.T) {
	gate := NewRecoveryGate()
	view := gate.Subscribe()
	var group sync.WaitGroup
	for session := range 8 {
		group.Add(1)
		go func() {
			defer group.Done()
			for range 1000 {
				attempt, refusal := gate.TryStart(strconv.Itoa(session), BackgroundWorkDelegationDelivery)
				if refusal != nil {
					t.Errorf("session %d refused by %q", session, refusal.Holders)
					return
				}
				attempt.Release()
			}
		}()
	}
	group.Wait()
	if !reflect.DeepEqual(view.Borrow(), gate.BusySessions()) {
		t.Fatalf("published %q, busy %q", view.Borrow(), gate.BusySessions())
	}
	if len(view.Borrow()) != 0 {
		t.Fatalf("published %q", view.Borrow())
	}
}

func TestAbortingABlockingWaiterKeepsAdmissionUntilExecutorSettles(t *testing.T) {
	gate := NewRecoveryGate()
	attempt := recoveryStart(t, gate, "session", BackgroundWorkDelegationDelivery)
	started := make(chan struct{})
	finish := make(chan struct{})
	ctx, cancel := context.WithCancel(context.Background())
	waited := make(chan error, 1)
	go func() {
		_, err, settlement := RecoveryRunBlocking(attempt, ctx, func(*RecoveryAttempt) struct{} {
			close(started)
			<-finish
			return struct{}{}
		})
		settlement.Release()
		waited <- err
	}()
	<-started
	cancel()
	if err := <-waited; !errors.Is(err, context.Canceled) {
		t.Fatalf("waiter = %v", err)
	}
	if !gate.IsBusy("session") {
		t.Fatal("aborting the waiter released a running executor")
	}
	gate.Close()
	close(finish)
	busy := gate.Subscribe()
	deadline, stop := context.WithTimeout(context.Background(), 5*time.Second)
	defer stop()
	if _, err := busy.WaitFor(deadline, func(sessions []string) bool { return len(sessions) == 0 }); err != nil {
		t.Fatalf("the executor kept admission: %v", err)
	}
}

func TestPanickingExecutorRetainsAdmissionUntilFailureIsSettled(t *testing.T) {
	gate := NewRecoveryGate()
	attempt := recoveryStart(t, gate, "session", BackgroundWorkDelegationDelivery)
	_, err, settlement := RecoveryRunBlocking(attempt, context.Background(), func(*RecoveryAttempt) struct{} {
		panic("executor failed")
	})
	if err == nil || !strings.Contains(err.Error(), "panicked") {
		t.Fatalf("result = %v", err)
	}
	recoveryRefused(t, gate, "session", BackgroundWorkDelegationDelivery)
	settlement.Release()
	recoveryStart(t, gate, "session", BackgroundWorkDelegationDelivery).Release()
}

func TestCoalescedObservationsKeepIndependentSessionsFair(t *testing.T) {
	sender, receiver := NewObservationChannel[int]()
	replace := func(int, *int) {}
	for value := range 100_000 {
		sender.Send("a", value, replace)
	}
	sender.Send("b", 7, replace)
	ctx := context.Background()
	if value, ok := receiver.Recv(ctx); !ok || value != 99_999 {
		t.Fatalf("first = %d, %v", value, ok)
	}
	sender.Send("a", 100_000, replace)
	if value, ok := receiver.Recv(ctx); !ok || value != 7 {
		t.Fatalf("second = %d, %v", value, ok)
	}
	if value, ok := receiver.Recv(ctx); !ok || value != 100_000 {
		t.Fatalf("third = %d, %v", value, ok)
	}
	if value, ok := receiver.TryRecv(); ok {
		t.Fatalf("left over %d", value)
	}
	sender.Close()
	if value, ok := receiver.Recv(ctx); ok {
		t.Fatalf("received %d after every sender closed", value)
	}
}

// A review waits for background delivery already running, then holds the
// session so no worker upgrade can start until it is done.
// Hard-won: 352a87453: a review hold blocked foreground work and daemon handoff.
func TestReservingWhenIdleWaitsForRunningWorkThenBlocksNewWork(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		gate := NewRecoveryGate()
		copy := recoveryStart(t, gate, "session-1", BackgroundWorkDelegationDelivery)
		waiting := make(chan *RecoveryReservation, 1)
		go func() {
			reservation, err := gate.ReserveWhenIdle(context.Background(), "session-1")
			if err != nil {
				t.Error(err)
			}
			waiting <- reservation
		}()
		synctest.Wait()
		if len(waiting) != 0 {
			t.Fatal("the review did not wait for running work")
		}
		if copy.Cancelled() {
			t.Fatal("the review preempted running work")
		}
		copy.Release()
		var reservation *RecoveryReservation
		select {
		case reservation = <-waiting:
		case <-time.After(5 * time.Second):
			t.Fatal("the reservation did not follow the background holders")
		}
		// An admitted review excludes new background work.
		recoveryRefused(t, gate, "session-1", BackgroundWorkQuotaScheduling)
		// No worker upgrade starts while the review holds the session.
		recoveryRefused(t, gate, "session-1", BackgroundWorkWorkerReplacement)
		// Other sessions are free.
		recoveryStart(t, gate, "session-2", BackgroundWorkDelegationDelivery).Release()
		for _, kind := range []BackgroundWork{BackgroundWorkRelayRecovery, BackgroundWorkWorkerObservation} {
			// A live review must allow worker repair and readiness settlement.
			repair := recoveryStart(t, gate, "session-1", kind)
			recoveryRefused(t, gate, "session-1", BackgroundWorkWorkerReplacement)
			repair.Release()
		}
		foreground, err := gate.EnterForeground(context.Background(), "session-1", "Move", LifecycleOriginPerson, new(atomic.Bool), ProcessExecutor{})
		if err != nil {
			t.Fatal(err)
		}
		reservation.Release()
		refusal := recoveryRefused(t, gate, "session-1", BackgroundWorkDelegationDelivery)
		if !reflect.DeepEqual(refusal.Holders, []string{"Move"}) {
			t.Fatalf("dropping the review must retain the foreground holder's identity: %q", refusal.Holders)
		}
		foreground.Release()
		recoveryStart(t, gate, "session-1", BackgroundWorkDelegationDelivery).Release()
	})
}

// recoveryNoticeLog records the notices an admission gives while it waits.
type recoveryNoticeLog struct {
	mu      sync.Mutex
	notices []string
}

func (*recoveryNoticeLog) CancellationRequested() bool { return false }

func (l *recoveryNoticeLog) NotifyNotice(notice string) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.notices = append(l.notices, notice)
}

func (l *recoveryNoticeLog) recorded() []string {
	l.mu.Lock()
	defer l.mu.Unlock()
	return append([]string(nil), l.notices...)
}

type recoveryEntered struct {
	admission *ForegroundAdmission
	err       error
}

func recoveryEnterAsync(gate *RecoveryGate, label string, cancelled *atomic.Bool, notices recoveryNotices) <-chan recoveryEntered {
	entered := make(chan recoveryEntered, 1)
	go func() {
		admission, err := gate.EnterForeground(context.Background(), "session", label, LifecycleOriginPerson, cancelled, notices)
		entered <- recoveryEntered{admission, err}
	}()
	return entered
}

func TestForegroundAdmissionNamesAndWaitsForAllHoldersAndReleasesCancelledIntents(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		gate := NewRecoveryGate()
		notices := &recoveryNoticeLog{}
		copy := recoveryStart(t, gate, "session", BackgroundWorkDelegationDelivery)
		cancelled := new(atomic.Bool)
		cancelled.Store(true)
		if _, err := gate.EnterForeground(context.Background(), "session", "Move", LifecycleOriginPerson, cancelled, notices); err == nil {
			t.Fatal("a cancelled intent was admitted")
		}
		if copy.Cancelled() {
			t.Fatal("a cancelled intent preempted running work")
		}
		cancelled.Store(false)
		waiting := recoveryEnterAsync(gate, "Move", cancelled, notices)
		synctest.Wait()
		if len(waiting) != 0 {
			t.Fatal("the foreground did not wait for running work")
		}
		refusal := recoveryRefused(t, gate, "session", BackgroundWorkWorkerReplacement)
		if got := notices.recorded(); !reflect.DeepEqual(got, []string{"Waiting for delegation delivery to stop"}) {
			t.Fatalf("notices = %q", got)
		}
		if !reflect.DeepEqual(refusal.Holders, []string{"delegation delivery", "Move"}) {
			t.Fatalf("holders = %q", refusal.Holders)
		}
		if !copy.Cancelled() {
			t.Fatal("the foreground did not preempt running work")
		}
		copy.Release()
		first := <-waiting
		if first.err != nil {
			t.Fatal(first.err)
		}
		second := recoveryEnterAsync(gate, "close", cancelled, notices)
		synctest.Wait()
		if len(second) != 0 {
			t.Fatal("a second foreground ran beside the first")
		}
		first.admission.Release()
		closing := <-second
		if closing.err != nil {
			t.Fatal(closing.err)
		}
		recoveryRefused(t, gate, "session", BackgroundWorkWorkerReplacement)
		closing.admission.Release()
		copy = recoveryStart(t, gate, "session", BackgroundWorkDelegationDelivery)
		waiting = recoveryEnterAsync(gate, "Move", cancelled, notices)
		synctest.Wait()
		if len(waiting) != 0 {
			t.Fatal("the foreground did not wait for running work")
		}
		cancelled.Store(true)
		if entered := <-waiting; entered.err == nil {
			t.Fatal("a cancelled wait was admitted")
		}
		copy.Release()
		recoveryStart(t, gate, "session", BackgroundWorkWorkerReplacement).Release()
	})
}

func TestTryEnterForegroundRefusesWithoutWaitingOrPreempting(t *testing.T) {
	gate := NewRecoveryGate()
	held := gate.Reserve("session")
	if _, refusal := gate.TryEnterForeground("session", "purge untracked", LifecycleOriginPerson); refusal == nil {
		t.Fatal("a reserved session granted a foreground admission")
	} else if refusal.Cause != RefusalCauseSessionHeld {
		t.Fatalf("cause = %v", refusal.Cause)
	}
	held.Release()
	admission, refusal := gate.TryEnterForeground("session", "purge untracked", LifecycleOriginPerson)
	if refusal != nil {
		t.Fatalf("refused by %q", refusal.Holders)
	}
	admission.Release()

	attempt := recoveryStart(t, gate, "session", BackgroundWorkWorkerObservation)
	if _, refusal := gate.TryEnterForeground("session", "purge untracked", LifecycleOriginPerson); refusal == nil {
		t.Fatal("background work granted a foreground admission")
	} else if refusal.Cause != RefusalCauseSessionHeld {
		t.Fatalf("cause = %v", refusal.Cause)
	}
	if attempt.Cancelled() {
		t.Fatal("a refused foreground preempted background work")
	}
	attempt.Release()
	admission, refusal = gate.TryEnterForeground("session", "purge untracked", LifecycleOriginPerson)
	if refusal != nil {
		t.Fatalf("refused by %q", refusal.Holders)
	}
	admission.Release()

	review, err := gate.ReserveWhenIdle(context.Background(), "session")
	if err != nil {
		t.Fatal(err)
	}
	admission, refusal = gate.TryEnterForeground("session", "purge untracked", LifecycleOriginPerson)
	if refusal != nil {
		t.Fatalf("a review refused a foreground admission: %q", refusal.Holders)
	}
	admission.Release()
	review.Release()

	gate.Close()
	if _, refusal := gate.TryEnterForeground("session", "purge untracked", LifecycleOriginPerson); refusal == nil {
		t.Fatal("a closed gate granted a foreground admission")
	} else if refusal.Cause != RefusalCauseClosed {
		t.Fatalf("cause = %v", refusal.Cause)
	}
}
