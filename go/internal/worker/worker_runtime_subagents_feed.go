// Port of mj-worker/src/worker_runtime/subagents/feed.rs.

// Per-connection replication. The durable queue remains the only work owner.

//go:build unix

package worker

import "github.com/BrokkAi/mjolnir/go/internal/core"

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
	panic("unported: mj-worker/src/worker_runtime/subagents/feed.rs SubagentFeed::poll")
}
