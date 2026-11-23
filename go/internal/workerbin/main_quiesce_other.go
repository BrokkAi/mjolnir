// Part of the port of mj-worker-bin/src/main.rs: `worker quiesce` elsewhere.

//go:build !linux && !darwin

package workerbin

import "errors"

// requestQuiesce refuses: managed checkout quiescence needs Linux or macOS.
func requestQuiesce(string, string, string, []string) error {
	return errors.New("managed checkout quiescence requires Linux or macOS")
}
