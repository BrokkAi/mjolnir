// Port of mj-worker-staging/src/worker_binary/binary_source.rs
// (index_entry_path's file identity without a Unix stat).

//go:build !linux && !darwin

package workerstaging

import (
	"hash"
	"os"
)

// indexFileIdentity adds the source's modification time to an index key, as
// Rust does where std has no MetadataExt.
func indexFileIdentity(key hash.Hash, info os.FileInfo) bool {
	key.Write([]byte(info.ModTime().String()))
	return true
}
