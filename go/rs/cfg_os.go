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

// UsizeMax is `usize::MAX` for a port in which usize is int: the largest Go
// int, 2^63-1. Rust's is 2^64-1 on every platform Mjolnir supports. The two
// agree as bounds and sentinels; where Rust converts, prints or serializes the
// value, use UsizeMaxU64 (translation-rules.md, rule 3).
const UsizeMax = int(^uint(0) >> 1)

// UsizeMaxU64 is Rust's `usize::MAX` as the value Rust code can observe:
// `u64::try_from(usize::MAX)`, `usize::MAX as u64`, or `usize::MAX` printed or
// serialized, which is 18446744073709551615 on 64-bit targets.
const UsizeMaxU64 = uint64(1<<64 - 1)
