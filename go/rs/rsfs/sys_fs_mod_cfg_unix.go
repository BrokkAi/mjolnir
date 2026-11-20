// Port of std 1.99.1 src/sys/fs/mod.rs (the items that differ on Unix)

//go:build unix

package rsfs

func sysRemoveDirAll(path string) error {
	// FIXME: use with_native_path on all platforms
	return impRemoveDirAll(path)
}

func sysSymlink(original, link string) error {
	return withNativePathUnit(original, func(original nativePath) error {
		return withNativePathUnit(link, func(link nativePath) error { return impSymlink(original, link) })
	})
}

func sysCopy(from, to string) (uint64, error) {
	// FIXME: use with_native_path on all platforms
	return impCopy(from, to)
}

func sysExists(path string) (bool, error) {
	// FIXME: use with_native_path on all platforms
	return impExists(path)
}
