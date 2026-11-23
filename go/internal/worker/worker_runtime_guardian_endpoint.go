// Port of mj-worker/src/worker_runtime/guardian_endpoint.rs.

// Pending permission reviews belong to the worker, across relay
// connections.

package worker

import (
	"context"
	"errors"
	"log/slog"
	"slices"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// guardianPending is one review waiting for the daemon's verdict.
type guardianPending struct {
	request core.GuardianReviewRequest
	expires time.Time
	// reply carries the verdict to the waiting review.
	reply chan core.GuardianReviewResult
	// abandoned is closed once the waiting review gave up (Rust's receiver
	// dropped).
	abandoned chan struct{}
}

func (p *guardianPending) replyClosed() bool {
	select {
	case <-p.abandoned:
		return true
	default:
		return false
	}
}

// GuardianEndpoint holds the worker's pending guardian reviews for the
// daemon, across relay connections. Share it as *GuardianEndpoint (Rust
// an Arc); the zero value is ready to use (Rust Default).
type GuardianEndpoint struct {
	mu      sync.Mutex
	pending map[string]*guardianPending
	// permits counts the reviews holding one of the
	// core.MaxPendingGuardianReviews slots (Rust a Semaphore).
	permits int
	// changes counts the queue's changes (Rust a watch of u64). Created on
	// first use under mu.
	changes *core.Watch[uint64]
}

// changesWatch is the change counter, created on first use. The caller
// holds e.mu.
func (e *GuardianEndpoint) changesWatch() *core.Watch[uint64] {
	if e.changes == nil {
		e.changes = core.NewWatch[uint64](0)
	}
	return e.changes
}

// pendingCount is the number of queued reviews.
func (e *GuardianEndpoint) pendingCount() int {
	e.mu.Lock()
	defer e.mu.Unlock()
	return len(e.pending)
}

// Changes is the queue's change counter.
func (e *GuardianEndpoint) Changes() *core.Watch[uint64] {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.changesWatch()
}

// Requests are the reviews still waiting for a verdict, in request id
// order.
func (e *GuardianEndpoint) Requests() []core.GuardianReviewRequest {
	now := time.Now()
	e.mu.Lock()
	defer e.mu.Unlock()
	ids := make([]string, 0, len(e.pending))
	for id := range e.pending {
		ids = append(ids, id)
	}
	slices.Sort(ids)
	requests := []core.GuardianReviewRequest{}
	for _, id := range ids {
		pending := e.pending[id]
		if pending.expires.After(now) && !pending.replyClosed() {
			requests = append(requests, pending.request)
		}
	}
	return requests
}

// Complete delivers the daemon's verdict to the review waiting for it.
func (e *GuardianEndpoint) Complete(result core.GuardianReviewResult) {
	e.mu.Lock()
	pending, ok := e.pending[result.RequestID]
	delete(e.pending, result.RequestID)
	e.mu.Unlock()
	if !ok || !pending.expires.After(time.Now()) {
		return
	}
	select {
	case pending.reply <- result:
	case <-pending.abandoned:
		slog.Debug("guardian permission request was withdrawn before its response")
	}
}

// review queues request for the daemon and waits for its verdict until
// deadline.
func (e *GuardianEndpoint) review(ctx context.Context, request core.GuardianReviewRequest, deadline time.Time) (core.GuardianReviewResult, error) {
	const timeout = "the daemon guardian did not return a verdict before the relay deadline"
	if !time.Now().Before(deadline) {
		return core.GuardianReviewResult{}, errors.New(timeout)
	}
	e.mu.Lock()
	if e.permits >= core.MaxPendingGuardianReviews {
		e.mu.Unlock()
		// The reviewer shows only this outermost context, as Rust's Display
		// of the anyhow error does.
		return core.GuardianReviewResult{}, errors.New("too many pending guardian reviews")
	}
	e.permits++
	e.mu.Unlock()
	defer func() {
		e.mu.Lock()
		e.permits--
		e.mu.Unlock()
	}()
	id, err := core.NewSessionID()
	if err != nil {
		return core.GuardianReviewResult{}, err
	}
	slog.Debug("queued a guardian request for the daemon", "target", "mj_guardian",
		"request_id", id, "tool_call_id", request.ToolCallID)
	request.RequestID = id
	pending := &guardianPending{
		request:   request,
		expires:   deadline,
		reply:     make(chan core.GuardianReviewResult, 1),
		abandoned: make(chan struct{}),
	}
	e.mu.Lock()
	if e.pending == nil {
		e.pending = map[string]*guardianPending{}
	}
	e.pending[id] = pending
	changes := e.changesWatch()
	changes.SendReplace(changes.Borrow() + 1)
	e.mu.Unlock()
	defer func() {
		close(pending.abandoned)
		e.mu.Lock()
		delete(e.pending, id)
		e.mu.Unlock()
	}()
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	select {
	case result := <-pending.reply:
		return result, nil
	case <-timer.C:
		return core.GuardianReviewResult{}, errors.New(timeout)
	case <-ctx.Done():
		return core.GuardianReviewResult{}, ctx.Err()
	}
}
