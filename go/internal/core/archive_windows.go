//go:build windows

// Port of mj-core/src/archive.rs: Windows has no O_NOFOLLOW; the regular
// file check after opening still refuses anything else.

package core

const archiveNoFollow = 0
