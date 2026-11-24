// Port of mj-cli/src/dashboard/auto_afk.rs tests.
//
// Rust's #[tokio::test] runs on a current-thread runtime, where the reporter
// runs until its next await before the test resumes; these run in a synctest
// bubble and call synctest.Wait where Rust yields.

package cli

import (
	"context"
	"sync"
	"testing"
	"testing/synctest"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// recordedTransport records each reported generation and holds the first
// report until the test acknowledges it.
type recordedTransport struct {
	generations *core.Unbounded[uint64]
	mu          sync.Mutex
	// firstAck is taken by the first report; nil afterwards.
	firstAck chan struct{}
}

func (t *recordedTransport) report(ctx context.Context, generation uint64) error {
	if err := t.generations.Send(generation); err != nil {
		panic(err)
	}
	t.mu.Lock()
	ack := t.firstAck
	t.firstAck = nil
	t.mu.Unlock()
	if ack != nil {
		select {
		case <-ack:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	return nil
}

// startRecordedReporter runs runReporter over a fresh signal watch, with no
// interval and a two-second report deadline. stop aborts it and waits for
// it to end.
func startRecordedReporter() (signal *core.Watch[activitySignal], observed *core.Unbounded[uint64], ack chan struct{}, stop func()) {
	signal = core.NewWatch(activitySignal{enabled: true})
	signalDropped := make(chan struct{})
	receiver := newWatchReceiver(signal, signalDropped)
	errs := make(chan string, 1)
	observed = core.NewUnbounded[uint64]()
	ack = make(chan struct{})
	ctx, abort := context.WithCancel(context.Background())
	worker := core.Spawn(func() struct{} {
		runReporter(ctx, receiver, errs, &recordedTransport{generations: observed, firstAck: ack}, 0, 2*time.Second)
		return struct{}{}
	})
	return signal, observed, ack, func() {
		abort()
		<-worker.Done()
	}
}

func nextObservedGeneration(t *testing.T, observed *core.Unbounded[uint64]) uint64 {
	t.Helper()
	generation, ok := observed.Recv(context.Background())
	if !ok {
		t.Fatal("the reporter stopped reporting")
	}
	return generation
}

func TestSlowAckCoalescesBurstIntoOneTrailingRequest(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		signal, observed, ack, stop := startRecordedReporter()
		defer stop()

		signal.SendReplace(activitySignal{enabled: true, generation: 1})
		if generation := nextObservedGeneration(t, observed); generation != 1 {
			t.Fatalf("reported %d", generation)
		}
		signal.SendReplace(activitySignal{enabled: true, generation: 2})
		signal.SendReplace(activitySignal{enabled: true, generation: 3})
		close(ack)
		if generation := nextObservedGeneration(t, observed); generation != 3 {
			t.Fatalf("reported %d", generation)
		}
		synctest.Wait()
		if generation, ok, _ := observed.TryRecv(); ok {
			t.Fatalf("an extra report of %d", generation)
		}
	})
}

func TestAwayTransitionDiscardsPendingInputToo(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		signal, observed, ack, stop := startRecordedReporter()
		defer stop()

		signal.SendReplace(activitySignal{enabled: true, generation: 1})
		if generation := nextObservedGeneration(t, observed); generation != 1 {
			t.Fatalf("reported %d", generation)
		}
		signal.SendReplace(activitySignal{enabled: true, generation: 2})
		signal.SendReplace(activitySignal{enabled: true, away: true, generation: 2, discardedThrough: 2})
		close(ack)
		synctest.Wait()
		if generation, ok, _ := observed.TryRecv(); ok {
			t.Fatalf("a report of %d after going away", generation)
		}
	})
}
