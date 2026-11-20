// Port of mj-core/src/test_hooks.rs: the default build without `test-hooks`.

//go:build !testhooks

package core

// testHooksEnabled is false in the default build: ReachTestHook is a no-op.
const testHooksEnabled = false
