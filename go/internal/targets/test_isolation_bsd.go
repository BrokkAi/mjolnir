// Port of mj-targets/src/test_isolation.rs's wait_for_exit_without_reaping
// on the other Unix systems, which Go's bindings give no waitid.

//go:build unix && !linux && !darwin

package targets

func waitForExitWithoutReaping(int) {
	panic("isolated tests need waitid, which Go offers on Linux and macOS only")
}
