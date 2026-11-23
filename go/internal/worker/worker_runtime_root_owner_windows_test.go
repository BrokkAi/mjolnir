// Port of mj-worker/src/worker_runtime/root_owner.rs tests: the
// `cfg(not(unix))` branch, which has no forked child.

//go:build windows

package worker

import "testing"

func holdForkedLockCopy(t *testing.T, owner *WorkerRootOwner) func() {
	return func() {}
}
