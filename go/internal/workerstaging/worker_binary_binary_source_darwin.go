// Port of mj-worker-staging/src/worker_binary/binary_source.rs
// (index_entry_path's Unix file identity, macOS field names).

//go:build darwin

package workerstaging

import (
	"encoding/binary"
	"hash"
	"os"
	"syscall"
)

// indexFileIdentity adds the source's mtime, ctime (seconds and
// nanoseconds) and inode to an index key, as Rust's MetadataExt does; false
// when the metadata carries no Unix stat.
func indexFileIdentity(key hash.Hash, info os.FileInfo) bool {
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return false
	}
	for _, value := range []int64{
		int64(stat.Mtimespec.Sec), int64(stat.Mtimespec.Nsec), int64(stat.Ctimespec.Sec), int64(stat.Ctimespec.Nsec),
	} {
		key.Write(binary.LittleEndian.AppendUint64(nil, uint64(value)))
	}
	key.Write(binary.LittleEndian.AppendUint64(nil, uint64(stat.Ino)))
	return true
}
