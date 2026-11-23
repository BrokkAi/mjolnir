// Part of the port of mj-worker-bin/src/main.rs: its `cfg(unix)` arms.

//go:build unix

package workerbin

import (
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// acquireRootOwner takes ownership of a worker run's root: the lock
// descriptor the login re-exec carried, or the lock itself, then prepares
// startup.
func acquireRootOwner(root string, lockFD *int32, loginEnvironmentReady bool) (*worker.WorkerRootOwner, error) {
	var owner *worker.WorkerRootOwner
	var err error
	if lockFD != nil {
		owner, err = worker.WorkerRootOwnerFromReexec(root, int(*lockFD))
	} else {
		owner, err = worker.WorkerRootOwnerAcquire(root)
	}
	if err != nil {
		return nil, err
	}
	if err := owner.PrepareStartup(loginEnvironmentReady); err != nil {
		owner.Release()
		return nil, err
	}
	return owner, nil
}

// bootstrapLoginEnvironmentReexec discovers the target's login environment
// (or, when needsLogin is false, only the bootstrap environment a
// checkpoint-only worker needs), applies the launch's image environment,
// target settings and credential policy, and re-executes the worker with
// exactly that environment, `--login-environment-ready` and, for a worker
// run, `--worker-root-lock-fd`. It returns only when the exec failed; the
// root lock is then no longer inheritable.
//
// Lifecycle probes identify the installed `hel worker run --root` prefix,
// so the re-exec keeps every argument intact, argv[0] included. On Linux it
// executes /proc/self/exe.
func bootstrapLoginEnvironmentReexec(command workerCommand, needsLogin bool, owner *worker.WorkerRootOwner) error {
	panic("unported: mj-worker-bin/src/main.rs bootstrap_login_environment (unix re-exec)")
}
