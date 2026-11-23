// Part of the port of mj-worker-bin/src/main.rs: `worker quiesce` on Linux
// and macOS.

//go:build linux || darwin

package workerbin

import "github.com/BrokkAi/mjolnir/go/internal/worker"

// requestQuiesce asks the live worker that owns root to stop every process
// for a managed suspend of checkout.
func requestQuiesce(root, operation, checkout string, childSessions []string) error {
	return worker.RequestSuspend(root, operation, checkout, childSessions)
}
