// Part of the port of mj-worker-bin/src/main.rs: a build without Cargo's
// `test-hooks` feature (no `testhooks` build tag).

//go:build !testhooks

package workerbin

// testHooksFeature is whether this build has the `test-hooks` feature.
const testHooksFeature = false
