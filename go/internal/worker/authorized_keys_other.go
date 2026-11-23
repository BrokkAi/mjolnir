// Port of mj-worker/src/authorized_keys.rs: elsewhere the file modes and
// the `flock` do not apply, as in Rust's `#[cfg(not(unix))]` branches.

//go:build !unix

package worker

import "os"

func chmodSSHDir(string) error { return nil }

func setFileMode0600(*os.File) error { return nil }

func authorizedKeysLockExclusive(*os.File) error { return nil }

func authorizedKeysUnlock(*os.File) {}
