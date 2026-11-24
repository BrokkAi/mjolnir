//go:build !linux && !darwin && !windows

// Port of mj-worker-launch/src/local_profile_homes.rs: sysinfo's process
// command lines on systems the Go build reads no process table on.

package workerlaunch

// processCommandLines lists nothing here, which
// runningProcessArgumentsWithoutPs reports as a listing that failed.
func processCommandLines() []string {
	return nil
}
