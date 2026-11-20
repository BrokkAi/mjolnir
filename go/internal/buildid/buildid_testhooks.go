// Port of the `test-hooks` part of mj-build-id/src/lib.rs and build.rs.

//go:build unix && testhooks

package buildid

// FakeWorkerDispatcher is a worker stand-in carrying this build's worker
// stamp, for a test in which a real program verifies the worker: core's
// checked-in fake command (testdata/test_hooks/fake-command.sh) with
// "\n#MJ-BUILD-ID:<WorkerBuildID>\n#\x00MJ-WORKER-BUILD:<WorkerBuildID>\x00\n"
// appended, mode 0755. Rust writes it at build time so executing it cannot
// race a test's writer (ETXTBSY); it must be written before tests start
// executing commands.
func FakeWorkerDispatcher() string {
	panic("unported: mj-build-id/src/lib.rs fake_worker_dispatcher")
}
