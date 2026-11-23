// Part of the port of mj-worker-bin/src/main.rs: its `cfg(not(unix))` arms.

//go:build !unix

package workerbin

import (
	"errors"

	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// acquireRootOwner takes ownership of a worker run's root. Without Unix
// there is no login re-exec, so no descriptor to adopt.
func acquireRootOwner(root string, _ *int32, _ bool) (*worker.WorkerRootOwner, error) {
	return worker.WorkerRootOwnerAcquire(root)
}

// bootstrapLoginEnvironmentReexec reports the missing worker: the target
// login environment crosses a Unix re-exec.
func bootstrapLoginEnvironmentReexec(workerCommand, bool, *worker.WorkerRootOwner) error {
	return errors.New("target login environment requires a Unix worker")
}
