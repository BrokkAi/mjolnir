// Part of the port of mj-worker-bin/src/main.rs: its `cfg(target_os =
// "linux")` arms.

package workerbin

import (
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/microvm"
)

// runMicrovm is `mj-worker microvm ...`: the Nitro Sandbox host engine and
// guest agent. They have their own command line (a Podman-compatible one),
// so they are dispatched before the worker's parser sees the arguments. It
// exits the process with the engine's status.
func runMicrovm() error {
	if err := installStderrLogging(); err != nil {
		return err
	}
	// Engine failures exit 125, as Podman's do.
	status, err := microvm.MicrovmMain(os.Args[2:])
	if err != nil {
		core.DiagnosticStderr("Error: %v", err)
		status = 125
	}
	os.Exit(status)
	return nil
}

// startOrphanReaper makes this worker the subreaper of its descendants and
// reaps the children the kernel reparents to it on every SIGCHLD, for as
// long as the process lives.
func startOrphanReaper() error {
	children := make(chan os.Signal, 1)
	signal.Notify(children, syscall.SIGCHLD)
	if err := core.OrphansBecomeSubreaper(); err != nil {
		signal.Stop(children)
		return err
	}
	go func() {
		for range children {
			if err := core.OrphansReapAdoptedChildren(); err != nil {
				slog.Warn("worker orphan reaper failed", "error", err)
			}
		}
	}()
	return nil
}
