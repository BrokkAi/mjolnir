// Port of std 1.99.1 src/sys/fs/mod.rs (the items that differ on Windows)

//go:build windows

package rsfs

func sysRemoveDirAll(path string) error {
	return withNativePathUnit(path, impRemoveDirAll)
}

func sysSymlink(original, link string) error {
	// FIXME: use with_native_path on all platforms
	return impSymlink(original, link)
}

func sysCopy(from, to string) (uint64, error) {
	return withNativePath(from, func(from nativePath) (uint64, error) {
		return withNativePath(to, func(to nativePath) (uint64, error) { return impCopy(from, to) })
	})
}

func sysExists(path string) (bool, error) {
	return withNativePath(path, impExists)
}
