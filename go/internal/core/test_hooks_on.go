// Port of mj-core/src/test_hooks.rs: the `test-hooks` feature.

//go:build testhooks

package core

// TestHooksEnabled is true in builds with the `testhooks` tag, Rust's
// `test-hooks` feature.
const TestHooksEnabled = true
