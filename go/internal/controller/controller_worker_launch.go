// Port of mj-controller/src/controller/worker_launch.rs (only what the
// dashboard calls so far; the body arrives with the Go daemon).

package controller

import "github.com/BrokkAi/mjolnir/go/internal/core"

// DiagnoseWorkerControlled probes the installed binary and the worker's
// recorded state after a session becomes unreachable, through executor.
// Returns nil only when the session has no target to probe; a probe that
// fails says why.
func (c *Controller) DiagnoseWorkerControlled(sessionID string, executor core.CommandExecutor) *string {
	panic("unported: mj-controller/src/controller/worker_launch.rs Controller::diagnose_worker_controlled")
}
