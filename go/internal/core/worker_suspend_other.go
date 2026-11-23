// Port of mj-core/src/worker_suspend.rs: WorkerIdentity::is_alive elsewhere.

//go:build !linux && !darwin

package core

import "errors"

func workerIdentityIsAlive(WorkerIdentity) (bool, error) {
	return false, errors.New("worker process identity requires Linux or macOS")
}
