// Port of the Unix parts of mj-targets/src/termination.rs.

//go:build unix

package targets

func installPlatformSignals(coordinator *Coordinator) {
	installUnixSignals(coordinator)
}

// finishLoginPlatform: only Windows holds a close handler for login.
func finishLoginPlatform(*Coordinator) {}

// installUnixSignals delivers SIGINT and SIGTERM (and SIGHUP for login) to
// the coordinator. The suppression decision is made when the signal is
// delivered; outside login, a signal after the first exits at once with
// 128 + signal, and SIGHUP exits at once.
func installUnixSignals(coordinator *Coordinator) {
	panic("unported: mj-targets/src/termination.rs install_unix_signals")
}

func exitCode(signal int) int {
	return 128 + signal
}
