// Port of mj-worker/src/acp/tests.rs: what its #[cfg(not(unix))] code sees
// where the Unix tests use FIFOs.

//go:build !unix

package worker

import "errors"

// acpTestsHasFifos says whether this platform has the FIFOs the fifo-load
// phase stalls a read on (Rust: cfg(unix)).
const acpTestsHasFifos = false

func acpTestsMkfifo(string) error {
	return errors.New("FIFOs need Unix")
}
