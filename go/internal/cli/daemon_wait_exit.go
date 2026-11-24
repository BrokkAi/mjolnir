// Port of mj-cli/src/daemon/wait_exit.rs.
//
// Wait for one daemon process and record its abnormal exit
// (`mj daemon-wait-exit`).

package cli

import (
	"os"
	"syscall"
)

// daemonWaitExitArgs are the arguments of `mj daemon-wait-exit` (Rust
// `DaemonWaitExitArgs`, a clap Args struct): `--exit-dir <EXIT_DIR>` and
// the required trailing command, which may begin with hyphens
// (`trailing_var_arg`, `allow_hyphen_values`).
type daemonWaitExitArgs struct {
	exitDir string
	command []string
}

// waiterSignals (Unix) are ignored by the waiter and restored to their
// defaults in the daemon it starts.
var waiterSignals = []os.Signal{syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP}

// waitExitRun runs the daemon command, waits for it, and records an exit
// other than 0 or core.StoreOwnedExitCode in args.exitDir (Rust
// `wait_exit::run`).
func waitExitRun(args daemonWaitExitArgs) error {
	panic("unported: mj-cli/src/daemon/wait_exit.rs run")
}
