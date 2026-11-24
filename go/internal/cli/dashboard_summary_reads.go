// Port of mj-cli/src/dashboard/summary_reads.rs.

package cli

import (
	"maps"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// readKey is one read summaryReads owns (sealed: readKeySummary,
// readKeyMessage). Both variants are comparable, so a readKey is a map key.
type readKey interface {
	isReadKey()
}

// readKeySummary is Rust's ReadKey::Summary: a stored session summary.
type readKeySummary struct{ id string }

// readKeyMessage is Rust's ReadKey::Message: a located notification
// message.
type readKeyMessage struct{ request tui.NotificationReadRequest }

func (readKeySummary) isReadKey() {}
func (readKeyMessage) isReadKey() {}

// compareReadKey is ReadKey's derived Ord: Summary before Message, then by
// value.
func compareReadKey(a, b readKey) int {
	switch a := a.(type) {
	case readKeySummary:
		if b, ok := b.(readKeySummary); ok {
			return strings.Compare(a.id, b.id)
		}
		return -1
	case readKeyMessage:
		if b, ok := b.(readKeyMessage); ok {
			return a.request.Compare(b.request)
		}
		return 1
	}
	return 0
}

// summaryRead is Rust's Read: one read's generation and its task.
type summaryRead struct {
	generation uint64
	// task is the read (Rust's AbortOnDropHandle): releasing the read
	// aborts it. Nil once finished.
	task *dashboardTask
}

// release is dropping the Rust handle, which aborts the read.
func (r *summaryRead) release() {
	if r.task != nil {
		r.task.abort()
		r.task = nil
	}
}

// summaryReads owns summary and located-message requests for the current
// view. Leaving it cancels queued reads; generations reject answers already
// delivered by an earlier visit.
type summaryReads struct {
	reads      map[readKey]*summaryRead
	generation uint64
	// permits is the Arc<Semaphore> of four network reads: a buffered
	// channel holding one token per admitted read.
	permits chan struct{}
}

// newSummaryReads is Rust's Default.
func newSummaryReads() *summaryReads {
	return &summaryReads{
		reads:   map[readKey]*summaryRead{},
		permits: make(chan struct{}, 4),
	}
}

// reconcile makes the owned reads exactly wanted (session id to the event
// ordinal viewed) plus item: it cancels the others, starts the missing ones,
// and answers the sessions whose summary reads it cancelled, in order.
func (s *summaryReads) reconcile(wanted map[string]uint64, item *tui.NotificationReadRequest, updates *core.Unbounded[dashboardIoUpdate]) []string {
	requests := make(map[readKey]uint64, len(wanted)+1)
	for id, viewed := range wanted {
		requests[readKeySummary{id: id}] = viewed
	}
	if item != nil {
		requests[readKeyMessage{request: *item}] = 0
	}
	var removed []string
	for _, key := range slices.SortedFunc(maps.Keys(s.reads), compareReadKey) {
		if _, kept := requests[key]; kept {
			continue
		}
		s.reads[key].release()
		delete(s.reads, key)
		if summary, ok := key.(readKeySummary); ok {
			removed = append(removed, summary.id)
		}
	}
	for _, key := range slices.SortedFunc(maps.Keys(requests), compareReadKey) {
		if _, owned := s.reads[key]; owned {
			continue
		}
		s.generation++
		var task *dashboardTask
		switch key := key.(type) {
		case readKeySummary:
			task = spawnStoredSessionSummary(key.id, requests[key], s.generation, s.permits, updates)
		case readKeyMessage:
			task = spawnNotificationMessageRead(key.request, s.generation, s.permits, updates)
		}
		s.reads[key] = &summaryRead{generation: s.generation, task: task}
	}
	return removed
}

// loadingSessions is the sessions whose summary read is still running, in
// order.
func (s *summaryReads) loadingSessions() []string {
	var loading []string
	for _, key := range slices.SortedFunc(maps.Keys(s.reads), compareReadKey) {
		if summary, ok := key.(readKeySummary); ok && s.reads[key].task != nil {
			loading = append(loading, summary.id)
		}
	}
	return loading
}

func (s *summaryReads) itemLoading(request tui.NotificationReadRequest) bool {
	read, ok := s.reads[readKeyMessage{request: request}]
	return ok && read.task != nil
}

func (s *summaryReads) finishItem(request tui.NotificationReadRequest, generation uint64) bool {
	return s.finishKey(readKeyMessage{request: request}, generation)
}

func (s *summaryReads) finish(id string, generation uint64) bool {
	return s.finishKey(readKeySummary{id: id}, generation)
}

func (s *summaryReads) finishKey(key readKey, generation uint64) bool {
	read, ok := s.reads[key]
	if !ok {
		return false
	}
	if read.generation != generation || read.task == nil {
		return false
	}
	// Rust drops the handle, which aborts a task that has already answered.
	read.release()
	return true
}
