// Port of mj-core/src/targets/diagnostics.rs.
//
// Diagnostic snapshots only; never use these to decide lifecycle admission.

package core

import (
	"sort"
	"sync"
	"sync/atomic"
	"time"
)

// BlockingOperationSnapshot is one running blocking operation as a stall
// reporter sees it.
type BlockingOperationSnapshot struct {
	Purpose   string
	Program   string
	Thread    string
	ElapsedMs uint64
}

type activeOperation struct {
	purpose string
	program string
	thread  string
	started time.Time
}

var (
	blockingOperationsMu   sync.Mutex
	blockingOperationsByID = map[uint64]activeOperation{}
	blockingOperationNext  atomic.Uint64
)

// BlockingOperation names a synchronous wait before it starts, including SSH
// admission and master preparation. Arguments, environment and stdin are
// never retained. Call End when the wait finishes (Rust: Drop).
type BlockingOperation struct {
	id uint64
}

// BlockingOperationStart registers a blocking operation (Rust:
// BlockingOperation::start).
func BlockingOperationStart(purpose, program string) *BlockingOperation {
	id := blockingOperationNext.Add(1)
	blockingOperationsMu.Lock()
	blockingOperationsByID[id] = activeOperation{
		purpose: purpose,
		program: program,
		// Go has no thread names or IDs; goroutines are anonymous.
		thread:  "unnamed goroutine",
		started: time.Now(),
	}
	blockingOperationsMu.Unlock()
	return &BlockingOperation{id: id}
}

// blockingOperationCommand registers command as a blocking operation
// (Rust: BlockingOperation::command).
func blockingOperationCommand(command *CommandSpec) *BlockingOperation {
	return BlockingOperationStart(command.Purpose, command.Program)
}

// End removes the operation from the registry (Rust: Drop).
func (o *BlockingOperation) End() {
	blockingOperationsMu.Lock()
	delete(blockingOperationsByID, o.id)
	blockingOperationsMu.Unlock()
}

// ActiveBlockingOperations never waits for the registry lock from a stall
// reporter. ok false means the snapshot was contended, rather than that no
// work was running.
func ActiveBlockingOperations() (snapshots []BlockingOperationSnapshot, ok bool) {
	if !blockingOperationsMu.TryLock() {
		return nil, false
	}
	defer blockingOperationsMu.Unlock()
	ids := make([]uint64, 0, len(blockingOperationsByID))
	for id := range blockingOperationsByID {
		ids = append(ids, id)
	}
	sort.Slice(ids, func(i, j int) bool { return ids[i] < ids[j] })
	snapshots = make([]BlockingOperationSnapshot, 0, len(ids))
	for _, id := range ids {
		operation := blockingOperationsByID[id]
		snapshots = append(snapshots, BlockingOperationSnapshot{
			Purpose:   operation.purpose,
			Program:   operation.program,
			Thread:    operation.thread,
			ElapsedMs: uint64(time.Since(operation.started).Milliseconds()),
		})
	}
	return snapshots, true
}
