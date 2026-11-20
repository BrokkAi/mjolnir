// Port of mj-core/src/worker_build.rs (platforms without a memo).

//go:build !linux && !darwin

package core

import "os"

// executableMemoSupported: path, size and mtime cannot distinguish
// same-size replacements with preserved timestamps on Windows, so the file is
// read each time.
const executableMemoSupported = false

func executableIdentityFromMetadata(info os.FileInfo) ExecutableIdentity {
	panic("executable identities are only read where executableMemoSupported")
}
