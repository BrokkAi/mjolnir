//go:build windows

// Port of the Windows part of mj-core/src/targets/storage.rs.

package core

// MeasureWindowsFilesystems is what StorageProbeScript reports, measured on
// Windows through the volume APIs, since Windows has no POSIX `df`. Each path
// is measured at its nearest existing ancestor, on the volume it is mounted
// from. A path that is not absolute here, such as a Linux container engine's
// `/var/lib/docker`, lives inside a VM that Windows cannot measure, and is
// left out like a path `df` cannot read.
func MeasureWindowsFilesystems(paths []string) []FilesystemSpace {
	panic("unported: mj-core/src/targets/storage.rs measure_windows_filesystems")
}
