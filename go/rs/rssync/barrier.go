// Port of std 1.99.1 src/sync/barrier.rs

package rssync

import (
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// barrierState is the inner state of a double barrier.
type barrierState struct {
	lock         sync.Mutex
	cvar         sync.Cond
	count        int
	generationId int
	numThreads   int
}

// Barrier is std::sync::Barrier: it lets several threads wait until all of
// them have arrived, and then releases them together.
//
// A Barrier is a small handle: copies are the same barrier, and the zero
// value is not one (create one with BarrierNew).
type Barrier struct {
	state *barrierState
}

// BarrierWaitResult is std::sync::BarrierWaitResult: what Wait returns to
// each thread of a group.
type BarrierWaitResult struct {
	isLeader bool
}

// BarrierNew is Barrier::new: a barrier that releases its threads n at a
// time. A barrier of 0 behaves as one of 1 and never blocks.
func BarrierNew(n int) Barrier {
	state := &barrierState{numThreads: n}
	state.cvar.L = &state.lock
	return Barrier{state: state}
}

// Wait is Barrier::wait: it blocks until n threads are waiting, and then
// releases them all. Exactly one of them is told that it is the leader.
func (b Barrier) Wait() BarrierWaitResult {
	state := b.state
	if state == nil {
		panicUninitialised("Barrier", 1)
	}
	state.lock.Lock()
	defer state.lock.Unlock()
	localGen := state.generationId
	state.count++
	if state.count < state.numThreads {
		for localGen == state.generationId {
			state.cvar.Wait()
		}
		return BarrierWaitResult{isLeader: false}
	}
	state.count = 0
	state.generationId++
	state.cvar.Broadcast()
	return BarrierWaitResult{isLeader: true}
}

// FmtDebug is the Debug impl: `Barrier { .. }`.
func (b Barrier) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Barrier").FinishNonExhaustive()
}

// IsLeader is BarrierWaitResult::is_leader: true for exactly one thread of
// each group that Wait released.
func (r BarrierWaitResult) IsLeader() bool { return r.isLeader }

// FmtDebug is the Debug impl: `BarrierWaitResult { is_leader: true }`.
func (r BarrierWaitResult) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("BarrierWaitResult").Field("is_leader", r.isLeader).Finish()
}
