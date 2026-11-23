// Part of the port of mj-worker-bin/src/main.rs: its `cfg(not(target_os =
// "linux"))` arms.

//go:build !linux

package workerbin

import "errors"

// runMicrovm refuses: the Nitro Sandbox engine exists only on Linux hosts.
func runMicrovm() error {
	return errors.New("mj-worker microvm runs only on Linux hosts")
}

// startOrphanReaper does nothing: only Linux has subreapers.
func startOrphanReaper() error { return nil }
