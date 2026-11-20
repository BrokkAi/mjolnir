// Port of mj-worker/src/worker_runtime/history.rs.

// Transient history requests. No history payload is written to worker
// state.

//go:build unix

package worker

import (
	"context"
	"log/slog"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// historyPending is one history query waiting for the controller's answer.
type historyPending struct {
	request core.HistoryRequest
	expires time.Time
	// reply is the oneshot sender (buffered for one result); closed is
	// closed once the waiting caller has gone (the receiver dropped).
	reply  chan core.HistoryResult
	closed <-chan struct{}
}

// replyClosed is Rust's oneshot::Sender::is_closed.
func (p *historyPending) replyClosed() bool {
	select {
	case <-p.closed:
		return true
	default:
		return false
	}
}

// historyEndpoint holds the history queries waiting for the controller.
// Copies of the pointer share it, as Rust's Clone of the Arcs does.
type historyEndpoint struct {
	mu      sync.Mutex
	pending map[string]*historyPending
	// permits is a semaphore of core.MaxPending tokens.
	permits chan struct{}
}

// defaultHistoryEndpoint is Rust's Default.
func defaultHistoryEndpoint() *historyEndpoint {
	return &historyEndpoint{
		pending: map[string]*historyPending{},
		permits: make(chan struct{}, core.MaxPending),
	}
}

// Requests are the live queries for the controller, after dropping expired
// and abandoned ones.
func (e *historyEndpoint) Requests() []core.HistoryRequest {
	panic("unported: mj-worker/src/worker_runtime/history.rs HistoryEndpoint::requests")
}

// Complete delivers the controller's answer to its waiting query.
func (e *historyEndpoint) Complete(result core.HistoryResult) {
	e.mu.Lock()
	entry, ok := e.pending[result.RequestID]
	delete(e.pending, result.RequestID)
	e.mu.Unlock()
	if !ok {
		return
	}
	if entry.replyClosed() {
		slog.Debug("history caller disconnected before its response")
		return
	}
	select {
	case entry.reply <- result:
	default:
		slog.Debug("history caller disconnected before its response")
	}
}

// Query queues a history query for the controller and waits for its answer.
func (e *historyEndpoint) Query(ctx context.Context, query core.HistoryQuery, cwd string) (core.HistoryResult, error) {
	panic("unported: mj-worker/src/worker_runtime/history.rs HistoryEndpoint::query")
}

func historyGit(ctx context.Context, cwd string, args []string) (string, error) {
	panic("unported: mj-worker/src/worker_runtime/history.rs git")
}

func captureBlame(ctx context.Context, cwd string, path string, start int, end int) (core.BlameEvidence, error) {
	panic("unported: mj-worker/src/worker_runtime/history.rs capture_blame")
}
