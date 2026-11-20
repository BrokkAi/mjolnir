package rs

import "runtime"

// The operating system predicates of `cfg!`, as constants so that the
// compiler removes the branch that does not apply.
const (
	// CfgWindows is `cfg!(windows)`.
	CfgWindows = runtime.GOOS == "windows"
	// CfgUnix is `cfg!(unix)`.
	CfgUnix = runtime.GOOS != "windows" && runtime.GOOS != "js" && runtime.GOOS != "wasip1" && runtime.GOOS != "plan9"
	// CfgMacos is `cfg!(target_os = "macos")`.
	CfgMacos = runtime.GOOS == "darwin"
	// CfgLinux is `cfg!(target_os = "linux")`.
	CfgLinux = runtime.GOOS == "linux"
)

// UsizeMax is `usize::MAX` for a port in which usize is int.
const UsizeMax = int(^uint(0) >> 1)
