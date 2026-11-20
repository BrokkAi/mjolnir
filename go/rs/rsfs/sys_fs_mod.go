// Port of std 1.99.1 src/sys/fs/mod.rs

package rsfs

import "github.com/BrokkAi/mjolnir/go/rs"

// withNativePathUnit is with_native_path for a call that returns ().
func withNativePathUnit(path string, f func(p nativePath) error) error {
	_, err := withNativePath(path, func(p nativePath) (rs.Unit, error) {
		return rs.Unit{}, f(p)
	})
	return err
}

func sysReadDirOf(path string) (*sysReadDir, error) {
	// FIXME: use with_native_path on all platforms
	return impReaddir(path)
}

func sysRemoveFile(path string) error {
	return withNativePathUnit(path, impUnlink)
}

func sysRename(old, new string) error {
	return withNativePathUnit(old, func(old nativePath) error {
		return withNativePathUnit(new, func(new nativePath) error { return impRename(old, new) })
	})
}

func sysRemoveDir(path string) error {
	return withNativePathUnit(path, impRmdir)
}

func sysReadLink(path string) (string, error) {
	return withNativePath(path, impReadlink)
}

func sysHardLink(original, link string) error {
	return withNativePathUnit(original, func(original nativePath) error {
		return withNativePathUnit(link, func(link nativePath) error { return impLink(original, link) })
	})
}

func sysMetadata(path string) (sysFileAttr, error) {
	return withNativePath(path, impStat)
}

func sysSymlinkMetadata(path string) (sysFileAttr, error) {
	return withNativePath(path, impLstat)
}

func sysSetPermissions(path string, perm sysFilePermissions) error {
	return withNativePathUnit(path, func(path nativePath) error { return impSetPerm(path, perm) })
}

func sysCanonicalize(path string) (string, error) {
	return withNativePath(path, impCanonicalize)
}
