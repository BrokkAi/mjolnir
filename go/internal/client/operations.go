// Port of mj-client/src/operations.rs.

// Supervised operation tracking shared by application hosts.

package client

import (
	"maps"
	"slices"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CriticalOperationTracker counts the daemon-owned operations that hold a
// handoff open. Copies share one tracker.
type CriticalOperationTracker struct {
	inner *criticalOperationTrackerInner
}

type criticalOperationTrackerInner struct {
	nextID     atomic.Uint64
	mu         sync.Mutex
	operations map[uint64]criticalOperation
	// changed carries a generation that moves on every begin and release.
	changed *core.Watch[uint64]
}

type criticalOperation struct {
	label     string
	cancelled *atomic.Bool
}

// CriticalOperationGuard holds one operation until Release (Rust's Drop).
type CriticalOperationGuard struct {
	id       uint64
	tracker  CriticalOperationTracker
	released atomic.Bool
}

// NewCriticalOperationTracker is an empty tracker and the watch of its
// generation, which moves whenever an operation begins or ends.
func NewCriticalOperationTracker() (CriticalOperationTracker, *core.Watch[uint64]) {
	changed := core.NewWatch[uint64](0)
	inner := &criticalOperationTrackerInner{operations: map[uint64]criticalOperation{}, changed: changed}
	inner.nextID.Store(1)
	return CriticalOperationTracker{inner: inner}, changed
}

func (t CriticalOperationTracker) Begin(label string) *CriticalOperationGuard {
	return t.beginInner(label, nil)
}

// BeginCancellable begins an operation that CancelAll can ask to stop by
// setting cancelled.
func (t CriticalOperationTracker) BeginCancellable(label string, cancelled *atomic.Bool) *CriticalOperationGuard {
	return t.beginInner(label, cancelled)
}

func (t CriticalOperationTracker) beginInner(label string, cancelled *atomic.Bool) *CriticalOperationGuard {
	id := t.inner.nextID.Add(1) - 1
	t.inner.mu.Lock()
	t.inner.operations[id] = criticalOperation{label: label, cancelled: cancelled}
	t.inner.bumpLocked()
	t.inner.mu.Unlock()
	return &CriticalOperationGuard{id: id, tracker: t}
}

// Blockers is the labels of the operations in flight, oldest first.
func (t CriticalOperationTracker) Blockers() []string {
	t.inner.mu.Lock()
	defer t.inner.mu.Unlock()
	ids := slices.Sorted(maps.Keys(t.inner.operations))
	labels := make([]string, 0, len(ids))
	for _, id := range ids {
		labels = append(labels, t.inner.operations[id].label)
	}
	return labels
}

// CancelAll asks every cancellable operation in flight to stop.
func (t CriticalOperationTracker) CancelAll() {
	t.inner.mu.Lock()
	defer t.inner.mu.Unlock()
	for _, operation := range t.inner.operations {
		if operation.cancelled != nil {
			operation.cancelled.Store(true)
		}
	}
}

// Release ends the operation. Releasing twice is harmless.
func (g *CriticalOperationGuard) Release() {
	if g.released.Swap(true) {
		return
	}
	inner := g.tracker.inner
	inner.mu.Lock()
	delete(inner.operations, g.id)
	inner.bumpLocked()
	inner.mu.Unlock()
}

// bumpLocked moves the generation on, wrapping as Rust's wrapping_add does.
// The caller holds mu.
func (i *criticalOperationTrackerInner) bumpLocked() {
	i.changed.SendReplace(i.changed.Borrow() + 1)
}
