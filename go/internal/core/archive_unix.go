//go:build unix

// Port of mj-core/src/archive.rs: native artifacts open without following a
// final symlink on Unix.

package core

import "syscall"

const archiveNoFollow = syscall.O_NOFOLLOW
