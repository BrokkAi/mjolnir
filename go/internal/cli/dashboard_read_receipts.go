// Port of mj-cli/src/dashboard/read_receipts.rs.
//
// Per-session receipt writes. Optimistic acknowledgement is distinct from
// durability, and one slow session must not hold up another session's save.

package cli

import (
	"fmt"
	"maps"
	"reflect"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// preserveReadPositions keeps, in each changed incoming record, a read
// frontier this terminal had already advanced past the incoming one.
func preserveReadPositions(incoming *core.SnapshotMap[string, core.SessionRecord], previous core.SnapshotMap[string, core.SessionRecord]) {
	type preserved struct {
		id       string
		frontier uint64
	}
	var keep []preserved
	sessionRecordEqual := func(a, b *core.SessionRecord) bool { return reflect.DeepEqual(a, b) }
	for id, record := range previous.Changes(*incoming, sessionRecordEqual) {
		if record == nil {
			continue
		}
		old := previous.GetShared(id)
		if old == nil {
			continue
		}
		if frontier := old.ViewedThroughEventOrdinal; frontier > record.ViewedThroughEventOrdinal {
			keep = append(keep, preserved{id: id, frontier: frontier})
		}
	}
	for _, entry := range keep {
		record := incoming.GetMut(entry.id)
		if record == nil {
			panic("changed record exists")
		}
		record.ViewedThroughEventOrdinal = entry.frontier
	}
}

// readReceipts is every session's receipt save. The zero value is Rust's
// Default.
type readReceipts struct {
	sessions map[string]*receipt
	// failures is the last error of each session whose latest save failed.
	failures map[string]string
}

type receipt struct {
	desired uint64
	saved   uint64
	// inFlight is the frontier a save in flight carries; nil when none.
	inFlight *uint64
	failures uint8
}

// readReceiptRetry is one (session, frontier) save retryFailed started.
type readReceiptRetry struct {
	sessionID string
	through   uint64
}

// start begins a save of the desired frontier, unless one is in flight,
// nothing is unsaved, or two saves in a row failed.
func (r *receipt) start() (uint64, bool) {
	if r.inFlight != nil || r.desired <= r.saved || r.failures >= 2 {
		return 0, false
	}
	through := r.desired
	r.inFlight = &through
	return through, true
}

// acknowledge records that the session was read through through; it
// answers the frontier to save now, if a save should start.
func (r *readReceipts) acknowledge(sessionID string, through uint64) (uint64, bool) {
	if r.sessions == nil {
		r.sessions = map[string]*receipt{}
	}
	entry := r.sessions[sessionID]
	if entry == nil {
		entry = &receipt{}
		r.sessions[sessionID] = entry
	}
	if through > entry.desired {
		entry.desired = through
		entry.failures = 0
	}
	return entry.start()
}

// complete records a save's result (Rust's Result<u64, String>): the
// frontier the daemon saved, or err. It answers the next frontier to save,
// if one should start.
func (r *readReceipts) complete(sessionID string, saved uint64, err error) (uint64, bool) {
	entry := r.sessions[sessionID]
	if entry == nil || entry.inFlight == nil {
		return 0, false
	}
	sent := *entry.inFlight
	entry.inFlight = nil
	if err == nil && saved >= sent {
		entry.saved = max(entry.saved, saved)
		entry.failures = 0
		delete(r.failures, sessionID)
	} else {
		entry.failures++
		message := fmt.Sprintf("save acknowledged only event %d, expected %d", saved, sent)
		if err != nil {
			message = err.Error()
		}
		if r.failures == nil {
			r.failures = map[string]string{}
		}
		r.failures[sessionID] = message
	}
	return entry.start()
}

func (r *readReceipts) isSaving() bool {
	for _, entry := range r.sessions {
		if entry.inFlight != nil {
			return true
		}
	}
	return false
}

// retryFailed starts one more save of every unsaved frontier with nothing in
// flight, in session order.
func (r *readReceipts) retryFailed() []readReceiptRetry {
	var retries []readReceiptRetry
	for _, id := range slices.Sorted(maps.Keys(r.sessions)) {
		entry := r.sessions[id]
		if entry.inFlight == nil && entry.desired > entry.saved {
			entry.failures = 1
			if through, ok := entry.start(); ok {
				retries = append(retries, readReceiptRetry{sessionID: id, through: through})
			}
		}
	}
	return retries
}
