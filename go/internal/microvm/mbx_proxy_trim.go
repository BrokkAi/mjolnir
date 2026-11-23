//go:build linux

// Port of mj-microvm/src/mbx_proxy/trim.rs.
//
// Keeps the host's mbx cache within its budget: the least recently read
// entries go first, and staging files abandoned for an hour are removed.

package microvm

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"golang.org/x/sys/unix"
)

const microvmMbxGiB uint64 = 1 << 30

// microvmMbxCacheBudget is mbx 1.30's automatic store budget: 5% of the
// filesystem rounded down to 5 GiB, with a 5 GiB floor, 500 GiB ceiling,
// and 20 GiB fallback when the filesystem cannot be measured (mbx's
// config.rs STORE_BUDGET).
func microvmMbxCacheBudget(cache string) uint64 {
	const increment = 5 * microvmMbxGiB
	total, err := MicrovmFilesystemBytes(cache)
	switch {
	case err != nil:
		fmt.Fprintf(os.Stderr, "determine the mbx cache filesystem size: %s; using a 20 GiB budget\n", err)
		return 20 * microvmMbxGiB
	case total == 0:
		return 20 * microvmMbxGiB
	}
	return min(max(total/100*5/increment*increment, increment), 500*microvmMbxGiB)
}

// microvmMbxTrim removes cache entries until the cache fits budget (the
// automatic budget when nil) and returns the bytes it freed.
func microvmMbxTrim(cache string, budget *uint64) (uint64, error) {
	scan, err := os.ReadDir(cache)
	if errors.Is(err, fs.ErrNotExist) {
		return 0, nil
	}
	if err != nil {
		return 0, fmt.Errorf("scan %s: %w", cache, microvmIOError(err))
	}
	limit := uint64(0)
	if budget != nil {
		limit = *budget
	} else {
		limit = microvmMbxCacheBudget(cache)
	}
	type entry struct {
		accessed unix.Timespec
		path     string
		size     uint64
	}
	var entries []entry
	var total, freed uint64
	now := time.Now()
	for _, item := range scan {
		path := filepath.Join(cache, item.Name())
		var stat unix.Stat_t
		if err := unix.Lstat(path, &stat); err != nil {
			// A proxy may have just published or discarded a staging file.
			if errors.Is(err, unix.ENOENT) {
				continue
			}
			return 0, fmt.Errorf("inspect %s: %w", path, microvmIOError(err))
		}
		if stat.Mode&unix.S_IFMT != unix.S_IFREG {
			continue
		}
		name := item.Name()
		size := uint64(stat.Size)
		if strings.HasPrefix(name, microvmMbxStagingPrefix) {
			modified := time.Unix(stat.Mtim.Unix())
			if age := now.Sub(modified); age >= time.Hour {
				removed, err := microvmMbxRemove(path)
				if err != nil {
					return 0, err
				}
				if removed {
					freed += size
				}
			}
		} else if len(name) == 64 && microvmMbxIsHex(name) {
			entries = append(entries, entry{accessed: stat.Atim, path: path, size: size})
			total += size
		}
	}

	sort.Slice(entries, func(i, j int) bool {
		a, b := entries[i], entries[j]
		if a.accessed != b.accessed {
			if a.accessed.Sec != b.accessed.Sec {
				return a.accessed.Sec < b.accessed.Sec
			}
			return a.accessed.Nsec < b.accessed.Nsec
		}
		if a.path != b.path {
			return a.path < b.path
		}
		return a.size < b.size
	})
	for _, entry := range entries {
		if total <= limit {
			break
		}
		removed, err := microvmMbxRemove(entry.path)
		if err != nil {
			return 0, err
		}
		if removed {
			freed += entry.size
		}
		total -= entry.size
	}
	return freed, nil
}

func microvmMbxIsHex(name string) bool {
	for i := 0; i < len(name); i++ {
		if !microvmIsASCIIHexDigit(name[i]) {
			return false
		}
	}
	return true
}

// microvmMbxRemove removes a cache entry; false when it was already gone.
func microvmMbxRemove(path string) (bool, error) {
	err := os.Remove(path)
	switch {
	case err == nil:
		return true, nil
	case errors.Is(err, fs.ErrNotExist):
		return false, nil
	}
	return false, fmt.Errorf("remove mbx cache entry %s: %w", path, microvmIOError(err))
}
