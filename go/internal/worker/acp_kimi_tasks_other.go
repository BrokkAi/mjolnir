// Port of mj-worker/src/acp/kimi_tasks.rs: platforms without a file
// identity.

//go:build !unix

package worker

import "os"

// kimiFileIdentityKnown: outside Unix, Rust reads no file identity, so a
// same-length rewrite is detected only by its modification time.
const kimiFileIdentityKnown = false

func kimiFileIdentity(os.FileInfo) (device, inode uint64) {
	return 0, 0
}
