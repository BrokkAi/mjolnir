// Port of mj-worker/src/worker_runtime/subagents/feed.rs.

// Per-connection replication. The durable queue remains the only work owner.

//go:build unix

package worker

import (
	"errors"
	"maps"
	"reflect"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// subagentFeed replicates the queue to one daemon connection in pages.
type subagentFeed struct {
	known             map[string]core.SubagentQueueChange
	pending           *core.SubagentQueuePage
	sequence          uint64
	pendingGeneration *uint64
	knownGeneration   *uint64
}

// defaultSubagentFeed is Rust's Default.
func defaultSubagentFeed() subagentFeed {
	return subagentFeed{known: map[string]core.SubagentQueueChange{}}
}

// HasUnpolled reports whether the queue holds a generation this connection
// has not been given in full.
func (f *subagentFeed) HasUnpolled(endpoint *SubagentEndpoint) bool {
	endpoint.state.mu.Lock()
	generation := endpoint.state.state.generation
	endpoint.state.mu.Unlock()
	return f.knownGeneration == nil || *f.knownGeneration != generation
}

// Poll returns the next page of queue changes, or the unacknowledged page
// again until `acknowledged` names its sequence.
func (f *subagentFeed) Poll(endpoint *SubagentEndpoint, acknowledged *uint64) (core.SubagentQueuePage, error) {
	if f.known == nil {
		f.known = map[string]core.SubagentQueueChange{}
	}
	if page := f.pending; page != nil {
		if acknowledged == nil || *acknowledged != page.Sequence {
			return subagentFeedClonePage(page), nil
		}
		var receipts []string
		for _, change := range page.Changes {
			if completed, ok := change.(core.SubagentQueueChangeCompleted); ok {
				receipts = append(receipts, completed.Completion.RequestID)
			}
		}
		if endpoint != nil {
			if err := endpoint.acknowledgeReplies(receipts, false); err != nil {
				return core.SubagentQueuePage{}, err
			}
		}
		for _, change := range page.Changes {
			if retired, ok := change.(core.SubagentQueueChangeRetired); ok {
				delete(f.known, retired.RequestIDValue)
			} else {
				f.known[change.RequestID()] = change
			}
		}
		if !page.HasMore {
			f.knownGeneration = f.pendingGeneration
		}
		f.pending = nil
	}
	current := map[string]core.SubagentQueueChange{}
	f.pendingGeneration = nil
	if endpoint != nil {
		endpoint.state.mu.Lock()
		state := &endpoint.state.state
		state.collections.count++
		now := time.Now()
		state.collections.last = &now
		generation := state.generation
		f.pendingGeneration = &generation
		if f.knownGeneration != nil && *f.knownGeneration == generation {
			endpoint.state.mu.Unlock()
			page := core.SubagentQueuePage{Sequence: f.sequence + 1, Changes: []core.SubagentQueueChange{}, HasMore: false}
			f.sequence = page.Sequence
			pending := subagentFeedClonePage(&page)
			f.pending = &pending
			return page, nil
		}
		for _, request := range state.requests {
			current[request.RequestID] = core.SubagentQueueChangeRequest{Request: request}
		}
		for _, receipt := range state.completions {
			current[receipt.RequestID] = core.SubagentQueueChangeCompleted{Completion: receipt}
		}
		endpoint.state.mu.Unlock()
	}
	var changes []core.SubagentQueueChange
	for _, id := range slices.Sorted(maps.Keys(current)) {
		if known, ok := f.known[id]; !ok || !reflect.DeepEqual(known, current[id]) {
			changes = append(changes, current[id])
		}
	}
	for _, id := range slices.Sorted(maps.Keys(f.known)) {
		if _, ok := current[id]; !ok {
			changes = append(changes, core.SubagentQueueChangeRetired{RequestIDValue: id})
		}
	}
	page := core.SubagentQueuePage{Sequence: f.sequence + 1, Changes: []core.SubagentQueueChange{}, HasMore: false}
	bytes := 1024
	for _, change := range changes {
		encoded, err := workerRuntimeMarshalJSON(change)
		if err != nil {
			return core.SubagentQueuePage{}, err
		}
		size := len(encoded) + 1
		if size+1024 > core.RelayReplayByteBudget {
			return core.SubagentQueuePage{}, errors.New("sub-agent queue entry exceeds the page budget")
		}
		if bytes+size > core.RelayReplayByteBudget {
			page.HasMore = true
			break
		}
		bytes += size
		page.Changes = append(page.Changes, change)
	}
	f.sequence = page.Sequence
	pending := subagentFeedClonePage(&page)
	f.pending = &pending
	return page, nil
}

// subagentFeedClonePage copies a page, as Rust's Clone does, so the pending
// copy never shares its change list with a caller.
func subagentFeedClonePage(page *core.SubagentQueuePage) core.SubagentQueuePage {
	clone := *page
	clone.Changes = slices.Clone(page.Changes)
	if clone.Changes == nil {
		clone.Changes = []core.SubagentQueueChange{}
	}
	return clone
}
