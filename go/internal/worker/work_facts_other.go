// Port of mj-worker/src/work_facts.rs: the scan_processes branch for
// platforms other than Linux and macOS.

//go:build !linux && !darwin

package worker

import "errors"

// readTable fails: this platform does not expose process environments.
func readTable(string) (ProcessTable, error) {
	return ProcessTable{}, errors.New("this platform does not expose process environments")
}
