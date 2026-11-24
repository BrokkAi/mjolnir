// Port of mj-cli/src/dashboard/auto_afk.rs.
//
// Coalesced reporting of operator input to the daemon.

package cli

import (
	"context"
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
	panic("unported: mj-cli/src/dashboard/auto_afk.rs DaemonActivityTransport::report")
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
	panic("unported: mj-cli/src/dashboard/auto_afk.rs AutoAfkReporter::new")
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
	panic("unported: mj-cli/src/dashboard/auto_afk.rs AutoAfkReporter::shutdown")
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
// at most one per interval, each bounded by timeout. It returns once
// signalDropped closes with nothing unseen, or when ctx ends (abort).
// Reports that fail send one message per distinct error to failures.
func runReporter(ctx context.Context, signal *core.Watch[activitySignal], signalDropped <-chan struct{}, failures chan<- string, transport activityTransport, interval time.Duration, timeout time.Duration) {
	panic("unported: mj-cli/src/dashboard/auto_afk.rs run_reporter")
}

func eligibleGeneration(current activitySignal, reported uint64) bool {
	return current.enabled &&
		!current.away &&
		current.generation > reported &&
		current.generation > current.discardedThrough
}
