//go:build mj_debug

package rs

// DebugAssertions is `cfg!(debug_assertions)`: true in builds made with the
// mj_debug tag, which is how the tests are built, as Rust's tests run in the
// development profile.
const DebugAssertions = true
