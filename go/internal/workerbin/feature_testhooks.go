// Part of the port of mj-worker-bin/src/main.rs: Cargo's `test-hooks`
// feature, which the Go build turns on with `-tags testhooks`.

//go:build testhooks

package workerbin

// testHooksFeature is whether this build has the `test-hooks` feature: Main
// enables core's crash-boundary hooks, and the login re-exec carries
// testHookEnvironment.
const testHooksFeature = true
