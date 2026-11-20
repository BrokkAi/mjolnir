//go:build !mj_debug

package rs

// DebugAssertions is `cfg!(debug_assertions)`: false in release builds.
const DebugAssertions = false
