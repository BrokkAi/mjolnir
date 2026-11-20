// Port of the branches of mj-worker/src/cpu_usage.rs for hosts other than
// Linux and macOS.

//go:build !linux && !darwin

package worker

import (
	"errors"
	"time"
)

func processTreeCpuTime(uint32) (time.Duration, error) {
	return 0, errors.New("CPU sampling is not supported on this platform")
}

// onlineCpus refuses here. Rust reads sysconf on every Unix, but Go has no
// sysconf without cgo, and workers run only on Linux and macOS hosts.
func onlineCpus() (uint32, error) {
	return 0, errors.New("CPU sampling is not supported on this platform")
}

func discoverContainerCpuCounter() (cpuCounter, error) {
	return nil, errors.New("container CPU sampling is not supported on this platform")
}
