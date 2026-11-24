// Port of mj-cli/src/dashboard/auto_afk.rs.
//
// Coalesced reporting of operator input to the daemon.

package cli

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"time"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	minReportInterval = time.Second
	reportTimeout     = 30 * time.Second
)

type activitySignal struct {
	enabled    bool
	away       bool
	generation uint64
	// discardedThrough: input at or below this value happened before the
	// latest disabled/away transition and must never be replayed after a
	// later re-enable.
	discardedThrough uint64
}

// activityTransport reports one input generation to the daemon. ctx carries
// the report's deadline; when it ends the report is abandoned, as Rust drops
// the future.
type activityTransport interface {
	report(ctx context.Context, generation uint64) error
}

type daemonActivityTransport struct {
	client *client.DaemonClient
}

func (t *daemonActivityTransport) report(ctx context.Context, generation uint64) error {
	// Keep the stream local until the acknowledgement arrives. If the
	// enclosing deadline cancels the report, the potentially half-consumed
	// connection is dropped with it.
	daemonClient := t.client
	t.client = nil
	if daemonClient == nil {
		connected, err := client.ConnectExisting(ctx)
		if err != nil {
			return err
		}
		daemonClient = connected
	}
	reply, err := daemonClient.Request(ctx, client.DaemonActionRecordOperatorActivity{})
	if err != nil {
		daemonClient.Close()
		return fmt.Errorf("record operator activity: %w", err)
	}
	if _, done := reply.(client.DaemonReplyDone); done {
		t.client = daemonClient
		return nil
	}
	daemonClient.Close()
	return fmt.Errorf("unexpected activity-report reply %s", client.DaemonReplyDebug(reply))
}

// autoAfkReporter is one supervised sender for the dashboard lifetime. The
// watch keeps only the latest signal, so input during a slow request
// produces one trailing report rather than a queue of per-event tasks.
type autoAfkReporter struct {
	// signal is the watch sender; nil once shutdown dropped it.
	signal *core.Watch[activitySignal]
	// signalDropped is closed when shutdown drops the sender, which the
	// reporter reads as the watch closing (core.Watch has no close).
	signalDropped chan struct{}
	// errors is the receiving end of Rust's mpsc::channel(1).
	errors chan string
	task   *core.JoinHandle[struct{}]
	// abort ends the reporter's context (JoinHandle::abort).
	abort context.CancelFunc
	state activitySignal
}

func newAutoAfkReporter(enabled bool, away bool) *autoAfkReporter {
	state := activitySignal{enabled: enabled, away: away}
	signal := core.NewWatch(state)
	signalDropped := make(chan struct{})
	receiver := newWatchReceiver(signal, signalDropped)
	errs := make(chan string, 1)
	ctx, abort := context.WithCancel(context.Background())
	task := core.Spawn(func() struct{} {
		defer abort()
		runReporter(ctx, receiver, errs, &daemonActivityTransport{}, minReportInterval, reportTimeout)
		return struct{}{}
	})
	return &autoAfkReporter{
		signal:        signal,
		signalDropped: signalDropped,
		errors:        errs,
		task:          task,
		abort:         abort,
		state:         state,
	}
}

func (r *autoAfkReporter) configure(enabled bool, away bool) {
	becameGated := (!enabled || away) && (r.state.enabled && !r.state.away)
	r.state.enabled = enabled
	r.state.away = away
	if becameGated {
		r.state.discardedThrough = r.state.generation
	}
	if r.signal != nil {
		r.signal.SendReplace(r.state)
	}
}

func (r *autoAfkReporter) setEnabled(enabled bool) {
	r.configure(enabled, r.state.away)
}

func (r *autoAfkReporter) input() {
	if !r.state.enabled || r.state.away {
		return
	}
	r.state.generation++
	if r.signal != nil {
		r.signal.SendReplace(r.state)
	}
}

// shutdown drops the sender, lets the reporter finish for up to two
// seconds, then aborts it.
func (r *autoAfkReporter) shutdown() {
	if r.signal != nil {
		r.signal = nil
		close(r.signalDropped)
	}
	task := r.task
	if task == nil {
		return
	}
	r.task = nil
	timer := time.NewTimer(2 * time.Second)
	defer timer.Stop()
	select {
	case <-task.Done():
		if _, err := task.Join(context.Background()); err != nil {
			slog.Warn("auto-AFK reporter task failed during shutdown", "error", err.Error())
		}
	case <-timer.C:
		r.abort()
	}
}

// drop is Rust's Drop: abort a reporter shutdown did not take.
func (r *autoAfkReporter) drop() {
	if r.task != nil {
		r.task = nil
		if r.abort != nil {
			r.abort()
		}
	}
}

// isOperatorInput is crossterm's Key, Mouse and Paste events: every key and
// mouse message, presses and releases alike.
func isOperatorInput(event tea.Msg) bool {
	switch event.(type) {
	case tea.KeyMsg, tea.MouseMsg, tea.PasteMsg:
		return true
	}
	return false
}

// runReporter reports each eligible generation the signal watch publishes,
// at most one per interval, each bounded by timeout. It returns once the
// sender is dropped with nothing unseen, or when ctx ends (abort). Reports
// that fail send one message per distinct error to failures, without
// waiting for room.
func runReporter(ctx context.Context, signal *watchReceiver[activitySignal], failures chan<- string, transport activityTransport, interval time.Duration, timeout time.Duration) {
	var reportedGeneration uint64
	var lastReport *time.Time
	var lastError *string
	for {
		if !signal.changed(ctx) {
			return
		}
		current := signal.borrowAndUpdate()
		if !eligibleGeneration(current, reportedGeneration) {
			continue
		}
		if lastReport != nil {
			if !sleepUntil(ctx, lastReport.Add(interval)) {
				return
			}
		}
		current = signal.borrowAndUpdate()
		if !eligibleGeneration(current, reportedGeneration) {
			continue
		}
		// Mark the generation before sending. A newer signal arriving
		// during the request remains visible in the watch for one trailing
		// report once this request completes.
		reportedGeneration = current.generation
		now := time.Now()
		lastReport = &now
		reportCtx, cancel := context.WithTimeout(ctx, timeout)
		err := transport.report(reportCtx, current.generation)
		expired := errors.Is(reportCtx.Err(), context.DeadlineExceeded)
		cancel()
		if ctx.Err() != nil {
			return
		}
		if err != nil && expired {
			// tokio::time::timeout's Elapsed under the Rust context.
			err = errors.New("daemon did not acknowledge operator activity before its deadline: deadline has elapsed")
		}
		if err == nil {
			lastError = nil
			continue
		}
		message := fmt.Sprintf("Could not report dashboard input: %v", err)
		if lastError == nil || *lastError != message {
			select {
			case failures <- message:
			default:
			}
			lastError = &message
		}
	}
}

func eligibleGeneration(current activitySignal, reported uint64) bool {
	return current.enabled &&
		!current.away &&
		current.generation > reported &&
		current.generation > current.discardedThrough
}

// sleepUntil is tokio::time::sleep_until under ctx: false when ctx ended
// first.
func sleepUntil(ctx context.Context, deadline time.Time) bool {
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	select {
	case <-timer.C:
		return true
	case <-ctx.Done():
		return false
	}
}

// watchReceiver is tokio's watch::Receiver over a core.Watch: it knows which
// publications it has seen, and its sender's drop. Like the receiver
// watch::channel returns, it counts the value current when it was made as
// seen, so it must be made before anything else can publish.
type watchReceiver[T any] struct {
	watch *core.Watch[T]
	// seen is closed by the first publication after the last one this
	// receiver marked seen.
	seen <-chan struct{}
	// senderDropped is closed when the sender is dropped.
	senderDropped <-chan struct{}
}

func newWatchReceiver[T any](watch *core.Watch[T], senderDropped <-chan struct{}) *watchReceiver[T] {
	return &watchReceiver[T]{watch: watch, seen: watch.Changed(), senderDropped: senderDropped}
}

// changed is Receiver::changed: it waits for a publication this receiver
// has not seen and marks it seen. It answers false once the sender is
// dropped with nothing unseen, or when ctx ends. As in tokio, an unseen
// publication wins over the sender's drop.
func (r *watchReceiver[T]) changed(ctx context.Context) bool {
	select {
	case <-r.seen:
	case <-r.senderDropped:
		select {
		case <-r.seen:
		default:
			return false
		}
	case <-ctx.Done():
		return false
	}
	r.seen = r.watch.Changed()
	return true
}

// borrowAndUpdate is the current value, marked seen.
func (r *watchReceiver[T]) borrowAndUpdate() T {
	r.seen = r.watch.Changed()
	return r.watch.Borrow()
}

// borrow is the current value, without marking it seen.
func (r *watchReceiver[T]) borrow() T {
	return r.watch.Borrow()
}
