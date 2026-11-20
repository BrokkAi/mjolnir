// Port of mj-worker/src/worker_runtime/root_owner.rs: the `cfg(unix)`
// methods (descriptor inheritance across the login re-exec and the startup
// log redirection).

//go:build unix

package worker

// SetReexecInheritance marks the lock descriptor inheritable or not. The
// descriptor is inherited only by login re-exec, never by harnesses.
func (o *WorkerRootOwner) SetReexecInheritance(inherit bool) (int, error) {
	panic("unported: mj-worker/src/worker_runtime/root_owner.rs WorkerRootOwner::set_reexec_inheritance")
}

// WorkerRootOwnerFromReexec recovers the descriptor carried by the worker's
// own login re-exec.
func WorkerRootOwnerFromReexec(root string, fd int) (*WorkerRootOwner, error) {
	panic("unported: mj-worker/src/worker_runtime/root_owner.rs WorkerRootOwner::from_reexec")
}

// PrepareStartup clears the previous diagnostics and redirects stderr into
// worker.log. Only the owner may replace the shared diagnostic log or
// startup records.
func (o *WorkerRootOwner) PrepareStartup(reexec bool) error {
	panic("unported: mj-worker/src/worker_runtime/root_owner.rs WorkerRootOwner::prepare_startup")
}
