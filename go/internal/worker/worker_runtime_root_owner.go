// Port of mj-worker/src/worker_runtime/root_owner.rs.

// Exclusive ownership of every mutable file in a worker root. The Unix-only
// methods are in worker_runtime_root_owner_unix.go.

package worker

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// WorkerRootOwner holds a worker root's `worker.lock` for as long as it
// lives. Release is Rust's Drop.
type WorkerRootOwner struct {
	root string
	file *core.FileLock
}

const probeHoldAllowance = 250 * time.Millisecond

// WorkerRootOwnerIsHeld reports whether a worker holds the lock of `root`
// right now. A missing lock file means no worker has ever run there, which
// is the same answer. The probe takes a shared lock and drops it at once, so
// concurrent probes agree and only a starting worker could notice it (see
// WorkerRootOwnerAcquire).
func WorkerRootOwnerIsHeld(root string) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/root_owner.rs WorkerRootOwner::is_held")
}

// WorkerRootOwnerAcquire creates the root if needed and takes its exclusive
// lock, refusing a root another worker (or a legacy worker's control
// socket) already owns.
func WorkerRootOwnerAcquire(root string) (*WorkerRootOwner, error) {
	panic("unported: mj-worker/src/worker_runtime/root_owner.rs WorkerRootOwner::acquire")
}

// Root is the canonical worker root.
func (o *WorkerRootOwner) Root() string { return o.root }

// Release gives up ownership of the root (Rust's Drop of the lock).
func (o *WorkerRootOwner) Release() {
	if o.file != nil {
		o.file.Drop()
	}
}
