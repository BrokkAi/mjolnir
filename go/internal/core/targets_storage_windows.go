//go:build windows

// Port of the Windows part of mj-core/src/targets/storage.rs.

package core

import (
	"os"
	"path/filepath"

	"golang.org/x/sys/windows"
)

// MeasureWindowsFilesystems is what StorageProbeScript reports, measured on
// Windows through the volume APIs, since Windows has no POSIX `df`. Each path
// is measured at its nearest existing ancestor, on the volume it is mounted
// from. A path that is not absolute here, such as a Linux container engine's
// `/var/lib/docker`, lives inside a VM that Windows cannot measure, and is
// left out like a path `df` cannot read.
func MeasureWindowsFilesystems(paths []string) []FilesystemSpace {
	measure := func(path string) (FilesystemSpace, bool) {
		existing := path
		for {
			if _, err := os.Stat(existing); err == nil {
				break
			}
			parent := filepath.Dir(existing)
			if parent == existing {
				return FilesystemSpace{}, false
			}
			existing = parent
		}
		name, err := windows.UTF16PtrFromString(existing)
		if err != nil {
			return FilesystemSpace{}, false
		}
		volume := make([]uint16, 1024)
		if err := windows.GetVolumePathName(name, &volume[0], uint32(len(volume))); err != nil {
			return FilesystemSpace{}, false
		}
		var available, total, free uint64
		if err := windows.GetDiskFreeSpaceEx(&volume[0], &available, &total, &free); err != nil {
			return FilesystemSpace{}, false
		}
		reserved := uint64(0)
		// Free space beyond the caller's quota, which it cannot write.
		if free > available {
			reserved = free - available
		}
		return FilesystemSpace{
			Mount:          windows.UTF16ToString(volume),
			AvailableBytes: available,
			TotalBytes:     total,
			ReservedBytes:  reserved,
			Paths:          []string{path},
		}, true
	}
	filesystems := []FilesystemSpace{}
	for _, path := range paths {
		if !filepath.IsAbs(path) {
			continue
		}
		if measured, ok := measure(path); ok {
			placeMeasuredPath(&filesystems, measured)
		}
	}
	return filesystems
}
