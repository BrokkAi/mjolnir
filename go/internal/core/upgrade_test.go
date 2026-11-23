// Port of the tests in mj-core/src/upgrade.rs.

package core

import (
	"context"
	"log/slog"
	"reflect"
	"strings"
	"sync"
	"testing"
	"testing/synctest"
	"time"
)

func upgradeEnter(t *testing.T, gate *Gate, label string) *UpgradeWork {
	t.Helper()
	work, err := gate.Enter(label)
	if err != nil {
		t.Fatalf("enter %s: %v", label, err)
	}
	return work
}

// upgradeDone reports whether done is closed once every goroutine in the
// bubble has blocked (Rust: futures::poll!).
func upgradeDone(done <-chan struct{}) bool {
	synctest.Wait()
	select {
	case <-done:
		return true
	default:
		return false
	}
}

func TestWebPushCutoverWaitsForTheLastStartupRecoveryHold(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		gate := NewGate()
		startup := upgradeEnter(t, gate, startupRecoveryLabel)
		recovery := startup.Clone()
		unrelated := upgradeEnter(t, gate, "other control work")
		waiting := make(chan struct{})
		go func() {
			_ = gate.waitForStartupRecovery(context.Background())
			close(waiting)
		}()
		if upgradeDone(waiting) {
			t.Fatal("ready while startup recovery holds")
		}
		startup.Release()
		if upgradeDone(waiting) {
			t.Fatal("ready while a clone still holds")
		}
		recovery.Release()
		if !upgradeDone(waiting) {
			t.Fatal("still waiting after the last hold")
		}
		if labels := gate.ActiveLabels(); !reflect.DeepEqual(labels, []string{"other control work"}) {
			t.Fatalf("labels = %q", labels)
		}
		unrelated.Release()
	})
}

func TestHandoffWaitsForEveryOwnerAndClosesAdmissionAtomically(t *testing.T) {
	gate := NewGate()
	gate.logger = slog.New(NewCapturedLog())
	provisioning := upgradeEnter(t, gate, "provisioning")
	for range 1000 {
		if gate.TryClose() {
			t.Fatal("closed while provisioning")
		}
	}
	steering := upgradeEnter(t, gate, "steering")
	provisioning.Release()
	if gate.TryClose() {
		t.Fatal("closed while steering")
	}
	steering.Release()
	if !gate.TryClose() {
		t.Fatal("did not close when idle")
	}
	if _, err := gate.Enter("new operation"); err == nil {
		t.Fatal("admitted after close")
	}
	if !gate.TryClose() {
		t.Fatal("handoff admission is idempotent")
	}
}

func TestActiveLabelsNameEachBlockerAndCountRepeats(t *testing.T) {
	gate := NewGate()
	if labels := gate.ActiveLabels(); len(labels) != 0 {
		t.Fatalf("labels = %q", labels)
	}
	first := upgradeEnter(t, gate, "session lifecycle")
	second := upgradeEnter(t, gate, "session lifecycle")
	request := upgradeEnter(t, gate, "client request")
	if labels := gate.ActiveLabels(); !reflect.DeepEqual(labels, []string{"client request", "session lifecycle x2"}) {
		t.Fatalf("labels = %q", labels)
	}
	second.Release()
	if labels := gate.ActiveLabels(); !reflect.DeepEqual(labels, []string{"client request", "session lifecycle"}) {
		t.Fatalf("labels = %q", labels)
	}
	first.Release()
	request.Release()
	if labels := gate.ActiveLabels(); len(labels) != 0 {
		t.Fatalf("labels = %q", labels)
	}
}

// Hard-won: c87e5e88: upgrades waited minutes without explaining the oldest blocker.
func TestBlockersCarryTheAgeOfEachLabelsOldestHold(t *testing.T) {
	gate := NewGate()
	oldest := upgradeEnter(t, gate, "session lifecycle")
	newer := upgradeEnter(t, gate, "session lifecycle")
	sync := upgradeEnter(t, gate, "SessionWiki sync")
	// Backdate rather than sleep: the oldest lifecycle hold began 65s ago.
	gate.mu.Lock()
	holds := gate.state.active["session lifecycle"]
	holds[oldest.registration.hold].began = holds[oldest.registration.hold].began.Add(-65 * time.Second)
	gate.mu.Unlock()
	var rendered []string
	for _, blocker := range gate.Blockers() {
		rendered = append(rendered, blocker.String())
	}
	if want := []string{"SessionWiki sync (0s)", "session lifecycle x2 (oldest 1m 05s)"}; !reflect.DeepEqual(rendered, want) {
		t.Fatalf("blockers = %q", rendered)
	}
	// Releasing the oldest hold makes the newer one the label's age.
	oldest.Release()
	lifecycle := gate.Blockers()[1]
	if lifecycle.Label != "session lifecycle" || lifecycle.Count != 1 {
		t.Fatalf("lifecycle = %v", lifecycle)
	}
	if lifecycle.Age >= time.Minute {
		t.Fatalf("lifecycle = %v", lifecycle)
	}
	newer.Release()
	sync.Release()
	if blockers := gate.Blockers(); len(blockers) != 0 {
		t.Fatalf("blockers = %v", blockers)
	}
}

// Hard-won: c87e5e88: long daemon handoffs gave no explanation of what they awaited.
func TestAWaitingHandoffNamesItsBlockersInTheLogAndReportsWhenItCloses(t *testing.T) {
	log := NewCapturedLog()
	gate := NewGate()
	gate.logger = slog.New(log)
	held := upgradeEnter(t, gate, "SessionWiki sync")
	for range 10 {
		if gate.TryClose() {
			t.Fatal("closed while held")
		}
	}
	waiting := log.At(slog.LevelInfo)
	if len(waiting) != 1 {
		t.Fatalf("a wait is named once, not on every attempt: %q", waiting)
	}
	if !strings.Contains(waiting[0], "SessionWiki sync (0s)") {
		t.Fatalf("the log names the blocker and its age: %q", waiting)
	}
	// Past the report interval the still-waiting handoff names it again.
	gate.mu.Lock()
	wait := *gate.state.handoffWait
	gate.state.handoffWait = &[2]time.Time{wait[0].Add(-waitReportInterval), wait[1].Add(-waitReportInterval)}
	gate.mu.Unlock()
	if gate.TryClose() {
		t.Fatal("closed while held")
	}
	waiting = log.At(slog.LevelInfo)
	if len(waiting) != 2 || !strings.Contains(waiting[1], "still waiting") {
		t.Fatalf("waiting = %q", waiting)
	}
	held.Release()
	if !gate.TryClose() {
		t.Fatal("did not close when idle")
	}
	closed := log.At(slog.LevelInfo)
	if len(closed) < 3 || !strings.Contains(closed[2], "closed admission") {
		t.Fatalf("the log says when the wait ended: %q", closed)
	}
}

// Hard-won: 4b1b60b5: new deferrable work could keep an upgrade waiting indefinitely.
func TestAWaitingHandoffRefusesDeferrableWorkButAdmitsControlWork(t *testing.T) {
	gate := NewGate()
	gate.logger = slog.New(NewCapturedLog())
	running := upgradeEnter(t, gate, "session lifecycle")
	deferrable, err := gate.EnterUnlessDraining("worker swap")
	if err != nil {
		t.Fatal(err)
	}
	if gate.IsDraining() {
		t.Fatal("draining before a handoff")
	}
	if gate.TryClose() {
		t.Fatal("closed while held")
	}
	if !gate.IsDraining() {
		t.Fatal("not draining while a handoff waits")
	}
	if _, err := gate.EnterUnlessDraining("worker swap"); err == nil {
		t.Fatal("admitted deferrable work while draining")
	}
	control := upgradeEnter(t, gate, "client request")
	running.Release()
	deferrable.Release()
	control.Release()
	if !gate.TryClose() {
		t.Fatal("did not close when idle")
	}
	if _, err := gate.EnterUnlessDraining("worker swap"); err == nil {
		t.Fatal("admitted deferrable work after close")
	}
}

func TestDrainingLapsesWhenTheHandoffStopsAsking(t *testing.T) {
	gate := NewGate()
	gate.logger = slog.New(NewCapturedLog())
	running := upgradeEnter(t, gate, "session lifecycle")
	if gate.TryClose() {
		t.Fatal("closed while held")
	}
	gate.ExpireDrainForTest()
	if gate.IsDraining() {
		t.Fatal("still draining after the lapse")
	}
	work, err := gate.EnterUnlessDraining("worker swap")
	if err != nil {
		t.Fatal(err)
	}
	work.Release()
	running.Release()
}

// Hard-won: bcbba06e: daemon restart interrupted accepted destroys and restored sessions as running.
func TestAHandoffWaitsForADestroyUntilTheBoundAndThenAbandonsItWithAWarning(t *testing.T) {
	log := NewCapturedLog()
	// The bound is long and the wait is backdated rather than slept, so
	// machine load cannot make the first assertions run past the bound.
	bound := 60 * time.Second
	gate := gateWithDestroyWaitBound(bound)
	gate.logger = slog.New(log)
	destroy, err := gate.EnterDestroy("aaaa1111")
	if err != nil {
		t.Fatal(err)
	}
	if labels := gate.ActiveLabels(); !reflect.DeepEqual(labels, []string{DestroyLabel}) {
		t.Fatalf("labels = %q", labels)
	}
	if gate.TryClose() {
		t.Fatal("a destroy in flight holds the handoff")
	}
	if !gate.IsDraining() || !gate.DestroysHoldTheStop() {
		t.Fatal("the destroy does not hold the stop")
	}
	if warnings := log.At(slog.LevelWarn); len(warnings) != 0 {
		t.Fatalf("warnings = %q", warnings)
	}
	gate.mu.Lock()
	if gate.state.destroyWaitStarted == nil {
		gate.mu.Unlock()
		t.Fatal("the wait began")
	}
	backdated := gate.state.destroyWaitStarted.Add(-bound - time.Second)
	gate.state.destroyWaitStarted = &backdated
	gate.mu.Unlock()
	if gate.DestroysHoldTheStop() {
		t.Fatal("the destroy holds the stop past the bound")
	}
	if !gate.TryClose() {
		t.Fatal("past the bound the gate proceeds")
	}
	named := false
	for _, line := range log.At(slog.LevelWarn) {
		named = named || strings.Contains(line, "aaaa1111")
	}
	if !named {
		t.Fatalf("the warning names the abandoned session: %q", log.At(slog.LevelWarn))
	}
	destroy.Release()
}

func TestOtherWorkStillHoldsTheHandoffPastTheDestroyBound(t *testing.T) {
	gate := gateWithDestroyWaitBound(0)
	gate.logger = slog.New(NewCapturedLog())
	destroy, err := gate.EnterDestroy("aaaa1111")
	if err != nil {
		t.Fatal(err)
	}
	lifecycle := upgradeEnter(t, gate, "session lifecycle")
	if gate.TryClose() {
		t.Fatal("closed while lifecycle work holds")
	}
	lifecycle.Release()
	if !gate.TryClose() {
		t.Fatal("did not close past the destroy bound")
	}
	destroy.Release()
}

func TestAFinishedDestroyLetsTheHandoffCloseWithoutAWarning(t *testing.T) {
	log := NewCapturedLog()
	gate := gateWithDestroyWaitBound(60 * time.Second)
	gate.logger = slog.New(log)
	destroy, err := gate.EnterDestroy("aaaa1111")
	if err != nil {
		t.Fatal(err)
	}
	if gate.TryClose() {
		t.Fatal("closed while a destroy runs")
	}
	destroy.Release()
	if gate.DestroysHoldTheStop() {
		t.Fatal("a finished destroy holds the stop")
	}
	if !gate.TryClose() {
		t.Fatal("did not close")
	}
	if warnings := log.At(slog.LevelWarn); len(warnings) != 0 {
		t.Fatalf("warnings = %q", warnings)
	}
}

func TestClonesOfOneOperationCountOnceAndHoldUntilTheLastDrops(t *testing.T) {
	gate := NewGate()
	gate.logger = slog.New(NewCapturedLog())
	work := upgradeEnter(t, gate, "web action")
	clone := work.Clone()
	if labels := gate.ActiveLabels(); !reflect.DeepEqual(labels, []string{"web action"}) {
		t.Fatalf("labels = %q", labels)
	}
	work.Release()
	if gate.TryClose() {
		t.Fatal("closed while a clone holds")
	}
	clone.Release()
	if !gate.TryClose() {
		t.Fatal("did not close after the last clone")
	}
}

func TestConcurrentAdmissionEitherOwnsWorkOrObservesACommittedHandoff(t *testing.T) {
	for range 100 {
		gate := NewGate()
		gate.logger = slog.New(NewCapturedLog())
		var barrier sync.WaitGroup
		barrier.Add(2)
		admitted := make(chan *UpgradeWork, 1)
		refused := make(chan error, 1)
		go func() {
			barrier.Done()
			barrier.Wait()
			work, err := gate.Enter("request")
			admitted <- work
			refused <- err
		}()
		barrier.Done()
		barrier.Wait()
		closed := gate.TryClose()
		work, err := <-admitted, <-refused
		if closed == (err == nil) {
			t.Fatalf("closed %v, admitted %v", closed, err == nil)
		}
		work.Release()
		if !gate.TryClose() {
			t.Fatal("did not close after the request")
		}
	}
}

// Hard-won: 5b46f3845: queued client admission could not observe a draining handoff.
func TestDrainPublicationWakesExistingAndLateWaiters(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		gate := NewGate()
		gate.logger = slog.New(NewCapturedLog())
		held := upgradeEnter(t, gate, "client request")
		waiting := make(chan struct{})
		go func() {
			_ = gate.WaitUntilDraining(context.Background())
			close(waiting)
		}()
		if upgradeDone(waiting) {
			t.Fatal("draining before a handoff")
		}
		if gate.TryClose() {
			t.Fatal("closed while held")
		}
		if !upgradeDone(waiting) {
			t.Fatal("the drain did not wake the waiter")
		}
		if _, err := gate.EnterDestroyUnlessDraining("queued-destroy"); err == nil {
			t.Fatal("admitted a deferrable destroy while draining")
		}
		if err := gate.WaitUntilDraining(context.Background()); err != nil {
			t.Fatal(err)
		}
		resumed := make(chan bool, 1)
		go func() { resumed <- gate.WaitAfterDraining(context.Background()) }()
		synctest.Wait()
		if len(resumed) != 0 {
			t.Fatal("resumed while draining")
		}
		gate.ExpireDrainForTest()
		if !<-resumed {
			t.Fatal("lapse resumes background recovery without admission")
		}
		gate.BeginDraining()
		shutdown := make(chan bool, 1)
		go func() { shutdown <- gate.WaitAfterDraining(context.Background()) }()
		synctest.Wait()
		if len(shutdown) != 0 {
			t.Fatal("resumed while draining")
		}
		held.Release()
		if !gate.TryClose() {
			t.Fatal("did not close")
		}
		if <-shutdown {
			t.Fatal("closure retires the old daemon's observers")
		}

		gate = NewGate()
		gate.logger = slog.New(NewCapturedLog())
		closing := make(chan struct{})
		go func() {
			_ = gate.WaitUntilDraining(context.Background())
			close(closing)
		}()
		if upgradeDone(closing) {
			t.Fatal("draining before a handoff")
		}
		if !gate.TryClose() {
			t.Fatal("did not close")
		}
		if !upgradeDone(closing) {
			t.Fatal("closing did not wake the waiter")
		}
	})
}
