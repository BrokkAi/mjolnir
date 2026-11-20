// Port of mj-core/src/target_path.rs.

// Paths on the machine a session runs on.
//
// Every session target is a POSIX machine: a Linux container, an SSH or EC2
// host, or this machine when it is Unix (a Windows controller runs no local
// sessions). The controller keeps those paths as strings like its own, but
// they mean POSIX paths on every controller. On Windows, filepath.IsAbs
// requires a drive or UNC prefix and joining inserts `\`, so neither may
// decide or render a target path; these functions do.

package core

import (
	"path/filepath"
	"runtime"
	"strings"
)

// IsAbsolute is whether `path` starts at the POSIX root of its target. On
// Unix this is filepath.IsAbs.
func IsAbsolute(path string) bool {
	if runtime.GOOS != "windows" {
		return strings.HasPrefix(path, "/")
	}
	// Rust's first component is RootDir only without a drive or UNC prefix.
	return filepath.VolumeName(path) == "" && (strings.HasPrefix(path, "/") || strings.HasPrefix(path, `\`))
}

// IsAbsoluteOnHostOrTarget is whether `path` is absolute on this controller
// or on a session target. For records and drafts that may name either side,
// such as a mount source on a local or an SSH container host; the side that
// uses the path checks it again.
func IsAbsoluteOnHostOrTarget(path string) bool {
	return filepath.IsAbs(path) || IsAbsolute(path)
}

// Join joins `relative` onto `base`. When `base` is a target path, absolute
// or relative to the target user's home (as SSH and EC2 worker roots are),
// the result keeps POSIX separators on a Windows controller too, so it reads
// the same wherever it is shown or sent. Only a base this controller would
// call absolute, with a Windows drive or UNC prefix, is joined as a host
// path. Like Rust's Path::join, it does not clean the result.
func Join(base string, relative string) string {
	panic("unported: mj-core/src/target_path.rs join")
}

// Text is the POSIX text of a target path, for an argv element or a remote
// command. A path with a Windows drive or UNC prefix is this controller's
// own, as in Join, and keeps its spelling.
func Text(path string) string {
	// Only Windows joins components with `\`; on POSIX a backslash is an
	// ordinary file name character and must survive.
	if runtime.GOOS == "windows" && !filepath.IsAbs(path) {
		return strings.ReplaceAll(path, `\`, "/")
	}
	return path
}
