// Port of mj-worker/src/acp/antigravity_tasks.rs: platforms without a file
// identity.

//go:build !unix

package worker

import "os"

// antigravitySummaryIdentity: outside Unix, Rust identifies the summary
// database by its length alone.
func antigravitySummaryIdentity(meta os.FileInfo) [2]uint64 {
	return [2]uint64{0, uint64(meta.Size())}
}
