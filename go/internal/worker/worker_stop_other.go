// Port of the `cfg(not(any(target_os = "linux", target_os = "macos")))` stop
// in mj-worker/src/worker_stop.rs.

//go:build !linux && !darwin

package worker

import (
	"errors"
	"time"
)

// WorkerStopStop is worker_stop::stop, which only Linux and macOS hosts
// support.
func WorkerStopStop(string, time.Duration) error {
	return errors.New("workers run on Linux and macOS hosts only")
}
