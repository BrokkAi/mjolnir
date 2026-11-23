// The tests of mj-worker/src/exit_record.rs check the inode on Unix only.

//go:build !unix

package worker

import "testing"

// exitRecordInode has no inode to report outside Unix.
func exitRecordInode(*testing.T, string) (uint64, bool) { return 0, false }
